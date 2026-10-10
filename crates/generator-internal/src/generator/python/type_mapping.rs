use super::identifiers::sanitize_python_identifier;
use crate::error::{Error, Result};
use crate::generator::naming::{array_item_type_name, nested_type_name};
use crate::generator::types::NormalizedFieldNames;
use config::node::{PeppygenLanguage, QoSProfile, SchemaType, TopicRetention, TypeToken};
use indexmap::IndexMap;

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
                let fields = collect_python_fields(&object.fields, &class_name, nested_classes)?;
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
            let class_name = nested_type_name(struct_prefix, field_name);
            let fields = collect_python_fields(&object.fields, &class_name, nested_classes)?;
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
    collect_python_fields(&format.0, struct_prefix, nested_classes)
}

/// Maps the fields of the dataclass `class_name`, after checking that no two
/// of them sanitize to the same Python identifier. The dataclasses of nested
/// objects are pushed onto `nested_classes` as their fields are mapped, so
/// each one comes before the class that references it.
fn collect_python_fields(
    fields: &IndexMap<String, SchemaType>,
    class_name: &str,
    nested_classes: &mut Vec<NestedDataclass>,
) -> Result<Vec<PythonField>> {
    validate_python_identifier_collisions(fields.keys(), class_name)?;
    let mut python_fields = Vec::with_capacity(fields.len());
    for (field_name, schema) in fields {
        let type_str = schema_type_to_python(schema, class_name, field_name, nested_classes)?;
        python_fields.push(PythonField {
            name: sanitize_python_identifier(field_name),
            type_str,
            is_optional: schema.is_optional(),
        });
    }
    Ok(python_fields)
}

fn validate_python_identifier_collisions<'a, I>(field_names: I, context: &str) -> Result<()>
where
    I: IntoIterator<Item = &'a String>,
{
    let mut identifiers =
        NormalizedFieldNames::new(PeppygenLanguage::Python, "python identifier", context);
    for field_name in field_names {
        identifiers.claim(sanitize_python_identifier(field_name), field_name)?;
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

/// Returns the Python expression for a topic's retention.
pub fn retention_python(retention: TopicRetention) -> String {
    match retention {
        TopicRetention::LiveOnly => "peppylib.TopicRetention.live_only()".to_string(),
        TopicRetention::Latest { depth } => {
            format!("peppylib.TopicRetention.latest({})", depth.get())
        }
    }
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

    #[rstest::rstest]
    #[case::flat(
        r#"
        {
            "foo-bar": "u8",
            foo_bar: "u8"
        }
        "#,
        "Message"
    )]
    #[case::nested(
        r#"
        {
            status: {
                $type: "object",
                "foo-bar": "u8",
                foo_bar: "u8"
            }
        }
        "#,
        "MessageStatus"
    )]
    fn collect_fields_rejects_python_identifier_collisions(
        #[case] format: &str,
        #[case] expected_context: &str,
    ) {
        let format: MessageFormat = serde_json5::from_str(format).unwrap();

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
                assert_eq!(context, expected_context);
                assert_eq!(normalization, "python identifier");
                assert_eq!(normalized, "foo_bar");
                assert_eq!(first_field, "foo-bar");
                assert_eq!(second_field, "foo_bar");
            }
            other => panic!("expected FieldNameNormalizationCollision, got: {other:?}"),
        }
    }
}
