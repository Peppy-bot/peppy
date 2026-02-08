use crate::generator::naming::{sanitize_component, to_camel_case};
use config::node::{QoSProfile, SchemaType, TypeToken};

/// A field in a Python dataclass.
pub struct PythonField {
    pub name: String,
    pub type_str: String,
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
) -> String {
    let base_type = match schema {
        SchemaType::Type(token) => primitive_type_str(token).to_string(),
        SchemaType::Primitive(primitive) => primitive_type_str(&primitive.kind).to_string(),
        SchemaType::Array(array) => {
            let item_type = match array.items.as_ref().as_type_token() {
                Some(TypeToken::U8) => return wrap_optional(schema, "bytes".to_string()),
                Some(token) => primitive_type_str(token),
                None => panic!("unsupported nested schema type in array `{field_name}`"),
            };
            format!("list[{item_type}]")
        }
        SchemaType::Object(object) => {
            let class_name = format!("{struct_prefix}{}", to_camel_case(field_name));
            let mut fields = Vec::new();
            for (nested_name, nested_schema) in &object.fields {
                let field_type =
                    schema_type_to_python(nested_schema, &class_name, nested_name, nested_classes);
                fields.push(PythonField {
                    name: sanitize_component(nested_name),
                    type_str: field_type,
                });
            }
            nested_classes.push(NestedDataclass {
                name: class_name.clone(),
                fields,
            });
            class_name
        }
    };

    wrap_optional(schema, base_type)
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
) -> Vec<PythonField> {
    let mut fields = Vec::new();
    for (field_name, schema) in &format.0 {
        let type_str = schema_type_to_python(schema, struct_prefix, field_name, nested_classes);
        fields.push(PythonField {
            name: sanitize_component(field_name),
            type_str,
        });
    }
    fields
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

/// Maps a config type name string to a Python type string (for parameters).
pub fn type_name_to_python(type_name: &str) -> &'static str {
    match type_name {
        "bool" => "bool",
        "string" | "str" => "str",
        "bytes" => "bytes",
        "time" => "float",
        "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64" => "int",
        "f32" | "float" | "f64" | "double" => "float",
        _ => "str",
    }
}
