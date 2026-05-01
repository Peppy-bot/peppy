use super::identifiers::sanitize_python_identifier;
use crate::error::{Error, Result};
use crate::generator::naming::{array_item_type_name, to_camel_case};
use config::node::{PeppygenLanguage, QoSProfile, SchemaType, TypeToken};
use std::collections::HashMap;

/// A field in a Python dataclass.
pub struct PythonField {
    pub name: String,
    pub type_str: String,
    pub is_optional: bool,
}

/// A nested dataclass definition collected during type mapping.
pub struct NestedDataclass {
    pub name: String,
    pub fields: Vec<PythonField>,
}

/// Maps a `TypeToken` to its Python type string.
pub fn primitive_type_str(token: &TypeToken) -> &'static str {
    match token {
        TypeToken::Bool => "bool",
        TypeToken::String => "str",
        TypeToken::Bytes => "bytes",
        TypeToken::Time => "float",
        TypeToken::U8
        | TypeToken::U16
        | TypeToken::U32
        | TypeToken::U64
        | TypeToken::I8
        | TypeToken::I16
        | TypeToken::I32
        | TypeToken::I64 => "int",
        TypeToken::F32 | TypeToken::F64 => "float",
    }
}

/// Converts a `SchemaType` into a Python type string, collecting any nested
/// dataclass definitions that need to be emitted before use.
pub fn schema_type_to_python(
    schema: &SchemaType,
    struct_prefix: &str,
    field_name: &str,
    nested_classes: &mut Vec<NestedDataclass>,
) -> Result<String> {
    let base_type = match schema {
        SchemaType::Type(token) => primitive_type_str(token).to_string(),
        SchemaType::Primitive(primitive) => primitive_type_str(&primitive.kind).to_string(),
        SchemaType::Array(array) => match array.items.as_ref() {
            SchemaType::Object(object) => {
                let class_name = array_item_type_name(struct_prefix, field_name);
                validate_python_identifier_collisions(object.fields.keys(), &class_name)?;
                let mut fields = Vec::new();
                for (nested_name, nested_schema) in &object.fields {
                    let field_type = schema_type_to_python(
                        nested_schema,
                        &class_name,
                        nested_name,
                        nested_classes,
                    )?;
                    fields.push(PythonField {
                        name: sanitize_python_identifier(nested_name),
                        type_str: field_type,
                        is_optional: nested_schema.is_optional(),
                    });
                }
                nested_classes.push(NestedDataclass {
                    name: class_name.clone(),
                    fields,
                });
                format!("list[{class_name}]")
            }
            _ => {
                let item_type = match array.items.as_ref().as_type_token() {
                    Some(TypeToken::U8) => return Ok(wrap_optional(schema, "bytes".to_string())),
                    Some(token) => primitive_type_str(token),
                    None => {
                        return Err(Error::UnsupportedArrayItemSchema {
                            field: field_name.to_string(),
                        });
                    }
                };
                format!("list[{item_type}]")
            }
        },
        SchemaType::Object(object) => {
            let class_name = format!("{struct_prefix}{}", to_camel_case(field_name));
            validate_python_identifier_collisions(object.fields.keys(), &class_name)?;
            let mut fields = Vec::new();
            for (nested_name, nested_schema) in &object.fields {
                let field_type =
                    schema_type_to_python(nested_schema, &class_name, nested_name, nested_classes)?;
                fields.push(PythonField {
                    name: sanitize_python_identifier(nested_name),
                    type_str: field_type,
                    is_optional: nested_schema.is_optional(),
                });
            }
            nested_classes.push(NestedDataclass {
                name: class_name.clone(),
                fields,
            });
            class_name
        }
    };

    Ok(wrap_optional(schema, base_type))
}

fn wrap_optional(schema: &SchemaType, type_str: String) -> String {
    if schema.is_optional() {
        format!("Optional[{type_str}]")
    } else {
        type_str
    }
}

/// Collects typed fields from a `MessageFormat`, populating nested dataclass definitions.
pub fn collect_fields_from_format(
    format: &config::node::MessageFormat,
    struct_prefix: &str,
    nested_classes: &mut Vec<NestedDataclass>,
) -> Result<Vec<PythonField>> {
    validate_python_identifier_collisions(format.0.keys(), struct_prefix)?;
    let mut fields = Vec::new();
    for (field_name, schema) in &format.0 {
        let type_str = schema_type_to_python(schema, struct_prefix, field_name, nested_classes)?;
        fields.push(PythonField {
            name: sanitize_python_identifier(field_name),
            type_str,
            is_optional: schema.is_optional(),
        });
    }
    Ok(fields)
}

fn validate_python_identifier_collisions<'a, I>(field_names: I, context: &str) -> Result<()>
where
    I: IntoIterator<Item = &'a String>,
{
    let mut seen: HashMap<String, String> = HashMap::new();
    for field_name in field_names {
        let normalized = sanitize_python_identifier(field_name);
        if let Some(previous_field) = seen.get(&normalized) {
            if previous_field != field_name {
                return Err(Error::FieldNameNormalizationCollision {
                    language: PeppygenLanguage::Python,
                    context: context.to_string(),
                    normalization: "python identifier",
                    normalized,
                    first_field: previous_field.clone(),
                    second_field: field_name.clone(),
                });
            }
        } else {
            seen.insert(normalized, field_name.clone());
        }
    }
    Ok(())
}

/// Returns `true` if any field (direct or nested) uses `Optional[...]`.
pub fn uses_optional(fields: &[PythonField], nested_classes: &[NestedDataclass]) -> bool {
    fields.iter().any(|f| f.is_optional)
        || nested_classes
            .iter()
            .any(|c| c.fields.iter().any(|f| f.is_optional))
}

/// Returns the Python string for a QoS profile variant.
pub fn qos_profile_python(profile: &QoSProfile) -> &'static str {
    match profile {
        QoSProfile::SensorData => "peppylib.QoSProfile.SensorData",
        QoSProfile::Standard => "peppylib.QoSProfile.Standard",
        QoSProfile::Reliable => "peppylib.QoSProfile.Reliable",
        QoSProfile::Critical => "peppylib.QoSProfile.Critical",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::{ArrayKind, ArraySchema, MessageFormat, PeppygenLanguage};
    use indexmap::IndexMap;

    #[test]
    fn collect_fields_rejects_nested_array_item_shapes() {
        let invalid_items = SchemaType::Array(ArraySchema {
            kind: ArrayKind::Array,
            items: Box::new(SchemaType::Type(TypeToken::U8)),
            length: None,
            optional: false,
        });
        let invalid_array = SchemaType::Array(ArraySchema {
            kind: ArrayKind::Array,
            items: Box::new(invalid_items),
            length: None,
            optional: false,
        });
        let mut fields = IndexMap::new();
        fields.insert("samples".to_string(), invalid_array);
        let format = MessageFormat(fields);

        let mut nested = Vec::new();
        let result = collect_fields_from_format(&format, "Message", &mut nested);
        assert!(matches!(
            result,
            Err(Error::UnsupportedArrayItemSchema { field }) if field == "samples"
        ));
    }

    #[test]
    fn collect_fields_rejects_python_identifier_collisions() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                "foo-bar": "u8",
                foo_bar: "u8"
            }
            "#,
        )
        .unwrap();

        let mut nested = Vec::new();
        let err = match collect_fields_from_format(&format, "Message", &mut nested) {
            Ok(_) => panic!("expected FieldNameNormalizationCollision"),
            Err(err) => err,
        };
        match err {
            Error::FieldNameNormalizationCollision {
                language,
                context,
                normalization,
                normalized,
                first_field,
                second_field,
            } => {
                assert_eq!(language, PeppygenLanguage::Python);
                assert_eq!(context, "Message");
                assert_eq!(normalization, "python identifier");
                assert_eq!(normalized, "foo_bar");
                assert_eq!(first_field, "foo-bar");
                assert_eq!(second_field, "foo_bar");
            }
            other => panic!("expected FieldNameNormalizationCollision, got: {other:?}"),
        }
    }

    #[test]
    fn collect_fields_rejects_nested_python_identifier_collisions() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                status: {
                    $type: "object",
                    "foo-bar": "u8",
                    foo_bar: "u8"
                }
            }
            "#,
        )
        .unwrap();

        let mut nested = Vec::new();
        let err = match collect_fields_from_format(&format, "Message", &mut nested) {
            Ok(_) => panic!("expected FieldNameNormalizationCollision"),
            Err(err) => err,
        };
        match err {
            Error::FieldNameNormalizationCollision {
                language,
                context,
                normalization,
                normalized,
                first_field,
                second_field,
            } => {
                assert_eq!(language, PeppygenLanguage::Python);
                assert_eq!(context, "MessageStatus");
                assert_eq!(normalization, "python identifier");
                assert_eq!(normalized, "foo_bar");
                assert_eq!(first_field, "foo-bar");
                assert_eq!(second_field, "foo_bar");
            }
            other => panic!("expected FieldNameNormalizationCollision, got: {other:?}"),
        }
    }
}
