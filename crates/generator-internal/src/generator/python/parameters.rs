use super::code_builder::PythonCodeBuilder;
use super::identifiers::sanitize_python_identifier;
use super::type_mapping::primitive_type_str;
use crate::error::{Error, Result};
use crate::generator::naming::to_camel_case;
use crate::generator::rust::validate_parameter_schema as validate_parameters;
use config::ParameterSpec;

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
pub fn generate_python_parameters(parameters: &config::ParameterSchema) -> Result<String> {
    // Validate field names using the shared Rust validator
    validate_parameters(parameters)?;

    let mut builder = PythonCodeBuilder::new();
    let mut main_fields: Vec<ParameterField> = Vec::new();

    for (field_name, spec) in parameters {
        match spec {
            ParameterSpec::Group(_) => {
                let struct_name = to_camel_case(field_name);
                let field_ident = sanitize_python_identifier(field_name);
                main_fields.push(ParameterField {
                    original_key: field_name.clone(),
                    field_name: field_ident,
                    type_name: struct_name.clone(),
                    is_nested: true,
                });
                emit_nested_parameter_class(&mut builder, spec, &struct_name, field_name)?;
            }
            ParameterSpec::Primitive { kind, .. } => {
                let field_ident = sanitize_python_identifier(field_name);
                main_fields.push(ParameterField {
                    original_key: field_name.clone(),
                    field_name: field_ident,
                    type_name: primitive_type_str(kind).to_string(),
                    is_nested: false,
                });
            }
            ParameterSpec::Array { .. } => {
                return Err(Error::UnsupportedArrayParameter {
                    path: field_name.clone(),
                });
            }
        }
    }

    emit_parameter_dataclass(&mut builder, "Parameters", &main_fields);

    Ok(builder.build())
}

fn emit_nested_parameter_class(
    builder: &mut PythonCodeBuilder,
    spec: &ParameterSpec,
    class_name: &str,
    path: &str,
) -> Result<()> {
    let ParameterSpec::Group(fields) = spec else {
        return Err(Error::InvariantViolation {
            context: format!("expected parameter group at `{path}`"),
        });
    };

    // Recurse into nested groups first (so they're defined before referenced)
    for (field_name, field_spec) in fields {
        if let ParameterSpec::Group(_) = field_spec {
            let nested_name = nested_class_name(class_name, field_name);
            let nested_path = format!("{path}.{field_name}");
            emit_nested_parameter_class(builder, field_spec, &nested_name, &nested_path)?;
        }
    }

    let mut class_fields: Vec<ParameterField> = Vec::new();
    for (field_name, field_spec) in fields {
        let field_ident = sanitize_python_identifier(field_name);
        match field_spec {
            ParameterSpec::Primitive { kind, .. } => {
                class_fields.push(ParameterField {
                    original_key: field_name.clone(),
                    field_name: field_ident,
                    type_name: primitive_type_str(kind).to_string(),
                    is_nested: false,
                });
            }
            ParameterSpec::Group(_) => {
                let nested_name = nested_class_name(class_name, field_name);
                class_fields.push(ParameterField {
                    original_key: field_name.clone(),
                    field_name: field_ident,
                    type_name: nested_name,
                    is_nested: true,
                });
            }
            ParameterSpec::Array { .. } => {
                return Err(Error::UnsupportedArrayParameter {
                    path: format!("{path}.{field_name}"),
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
