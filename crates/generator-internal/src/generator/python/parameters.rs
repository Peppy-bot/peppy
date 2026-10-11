use super::code_builder::PythonCodeBuilder;
use super::identifiers::sanitize_python_identifier;
use super::type_mapping::primitive_type_str;
use crate::error::{Error, Result};
use crate::generator::naming::nested_type_name;
use crate::generator::rust::{
    join_parameter_path, validate_parameter_schema as validate_parameters,
};
use config::ParameterSpec;
use std::collections::BTreeMap;

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
    emit_parameter_group(&mut builder, parameters, "Parameters", "", "")?;
    Ok(builder.build())
}

/// Emits the dataclass `class_name` of one parameter group, after the
/// dataclasses of its nested groups. The class of a nested group is named
/// after its field, continuing `nested_prefix`, which is empty for the
/// top-level `Parameters` class. `path` is the dotted path of the group in
/// the schema, empty for the top level.
fn emit_parameter_group(
    builder: &mut PythonCodeBuilder,
    fields: &BTreeMap<String, ParameterSpec>,
    class_name: &str,
    nested_prefix: &str,
    path: &str,
) -> Result<()> {
    // Recurse into nested groups first (so they're defined before referenced)
    for (field_name, field_spec) in fields {
        if let ParameterSpec::Group(nested_fields) = field_spec {
            let nested_name = nested_type_name(nested_prefix, field_name);
            let nested_path = join_parameter_path(path, field_name);
            emit_parameter_group(
                builder,
                nested_fields,
                &nested_name,
                &nested_name,
                &nested_path,
            )?;
        }
    }

    let mut class_fields: Vec<ParameterField> = Vec::new();
    for (field_name, field_spec) in fields {
        let (type_name, is_nested) = match field_spec {
            ParameterSpec::Primitive { kind, .. } => (primitive_type_str(kind).to_string(), false),
            ParameterSpec::Group(_) => (nested_type_name(nested_prefix, field_name), true),
            ParameterSpec::Array { .. } => {
                return Err(Error::UnsupportedArrayParameter {
                    path: join_parameter_path(path, field_name),
                });
            }
        };
        class_fields.push(ParameterField {
            original_key: field_name.clone(),
            field_name: sanitize_python_identifier(field_name),
            type_name,
            is_nested,
        });
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
    builder.block(&format!("class {class_name}:"), |builder| {
        let from_dict_header = format!("def from_dict(cls, data: dict) -> \"{class_name}\":");
        if fields.is_empty() {
            // from_dict for an empty dataclass just returns cls()
            builder.line("@classmethod");
            builder.block(&from_dict_header, |builder| builder.line("return cls()"));
            return;
        }

        for field in fields {
            builder.line(&format!("{}: {}", field.field_name, field.type_name));
        }

        builder.blank_line();
        builder.line("@classmethod");
        builder.block(&from_dict_header, |builder| {
            builder.block("return cls(", |builder| {
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
            });
            builder.line(")");
        });
    });
    builder.blank_line();
}
