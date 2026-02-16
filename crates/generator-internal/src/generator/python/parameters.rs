use super::code_builder::PythonCodeBuilder;
use super::identifiers::sanitize_python_identifier;
use super::type_mapping::type_name_to_python;
use crate::error::{Error, Result};
use crate::generator::naming::to_camel_case;
use crate::generator::rust::validate_parameter_schema as validate_parameters;
use config::AnyType;

/// A parameter field with enough metadata to generate both the dataclass field
/// declaration and the `from_dict` conversion line.
struct ParameterField {
    /// Original key in the config dict (used for dict lookups in `from_dict`).
    original_key: String,
    /// Sanitized Python identifier (used as the dataclass field name).
    field_name: String,
    /// Python type name for the field annotation.
    type_name: String,
    /// Whether this field is a nested dataclass that needs recursive conversion.
    is_nested: bool,
}

/// Generates a Python `parameters.py` file from node parameters configuration.
///
/// Validates field names using the shared validator, then generates `@dataclass`
/// definitions (with `from_dict` classmethods) for all parameter groups.
pub fn generate_python_parameters(parameters: &config::NodeArguments) -> Result<String> {
    // Validate field names using the shared Rust validator
    validate_parameters(parameters)?;

    let mut builder = PythonCodeBuilder::new();

    // Emit nested classes in dependency order, then collect main fields
    let mut main_fields: Vec<ParameterField> = Vec::new();

    for (field_name, type_spec) in parameters {
        match type_spec {
            AnyType::Object(_) => {
                let struct_name = to_camel_case(field_name);
                let field_ident = sanitize_python_identifier(field_name);
                main_fields.push(ParameterField {
                    original_key: field_name.clone(),
                    field_name: field_ident,
                    type_name: struct_name.clone(),
                    is_nested: true,
                });
                emit_nested_parameter_class(&mut builder, type_spec, &struct_name, field_name)?;
            }
            AnyType::String(type_name) => {
                let field_ident = sanitize_python_identifier(field_name);
                let python_type = type_name_to_python(type_name, field_name)?;
                main_fields.push(ParameterField {
                    original_key: field_name.clone(),
                    field_name: field_ident,
                    type_name: python_type.to_string(),
                    is_nested: false,
                });
            }
            _ => {
                return Err(Error::UnsupportedParameterSpecType {
                    path: field_name.clone(),
                    kind: type_spec.type_name(),
                });
            }
        }
    }

    // Emit main Parameters class
    emit_parameter_dataclass(&mut builder, "Parameters", &main_fields);

    Ok(builder.build())
}

fn emit_nested_parameter_class(
    builder: &mut PythonCodeBuilder,
    type_spec: &AnyType,
    class_name: &str,
    path: &str,
) -> Result<()> {
    let AnyType::Object(fields) = type_spec else {
        return Err(Error::UnsupportedParameterSpecType {
            path: path.to_string(),
            kind: type_spec.type_name(),
        });
    };

    // Recurse into nested objects first (so they're defined before referenced)
    for (field_name, field_spec) in fields {
        if let AnyType::Object(_) = field_spec {
            let nested_name = nested_class_name(class_name, field_name);
            let nested_path = format!("{path}.{field_name}");
            emit_nested_parameter_class(builder, field_spec, &nested_name, &nested_path)?;
        }
    }

    let mut class_fields: Vec<ParameterField> = Vec::new();
    for (field_name, field_spec) in fields {
        let field_ident = sanitize_python_identifier(field_name);
        let field_path = format!("{path}.{field_name}");
        match field_spec {
            AnyType::String(type_name) => {
                let py_type = type_name_to_python(type_name, &field_path)?;
                class_fields.push(ParameterField {
                    original_key: field_name.clone(),
                    field_name: field_ident,
                    type_name: py_type.to_string(),
                    is_nested: false,
                });
            }
            AnyType::Object(_) => {
                let nested_name = nested_class_name(class_name, field_name);
                class_fields.push(ParameterField {
                    original_key: field_name.clone(),
                    field_name: field_ident,
                    type_name: nested_name,
                    is_nested: true,
                });
            }
            _ => {
                return Err(Error::UnsupportedParameterSpecType {
                    path: field_path,
                    kind: field_spec.type_name(),
                });
            }
        }
    }

    emit_parameter_dataclass(builder, class_name, &class_fields);
    Ok(())
}

/// Emits a `@dataclass` class with a `from_dict` classmethod that recursively
/// converts a plain dict (as delivered by the runtime) into a typed instance.
fn emit_parameter_dataclass(
    builder: &mut PythonCodeBuilder,
    class_name: &str,
    fields: &[ParameterField],
) {
    builder.add_import("from dataclasses import dataclass");
    builder.line("@dataclass");
    builder.line(&format!("class {class_name}:"));
    builder.indent();

    if fields.is_empty() {
        // from_dict for an empty dataclass just returns cls()
        builder.line("@classmethod");
        builder.line(&format!(
            "def from_dict(cls, data: dict) -> \"{class_name}\":"
        ));
        builder.indent();
        builder.line("return cls()");
        builder.dedent();
    } else {
        for field in fields {
            builder.line(&format!("{}: {}", field.field_name, field.type_name));
        }

        builder.blank_line();
        builder.line("@classmethod");
        builder.line(&format!(
            "def from_dict(cls, data: dict) -> \"{class_name}\":"
        ));
        builder.indent();
        builder.line("return cls(");
        builder.indent();
        for field in fields {
            if field.is_nested {
                builder.line(&format!(
                    "{}={}.from_dict(data[\"{}\"]),",
                    field.field_name, field.type_name, field.original_key
                ));
            } else {
                builder.line(&format!(
                    "{}=data[\"{}\"],",
                    field.field_name, field.original_key
                ));
            }
        }
        builder.dedent();
        builder.line(")");
        builder.dedent();
    }

    builder.dedent();
    builder.blank_line();
}

fn nested_class_name(parent_class: &str, field_name: &str) -> String {
    format!("{parent_class}{}", to_camel_case(field_name))
}
