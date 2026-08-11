//! The canonical, versioned mapping from `message_format` definitions to the
//! JSON Schemas published over MCP.
//!
//! The `message_format` DSL is closed and small, so the mapping is mechanical
//! with four explicit rules for the shapes JSON cannot carry natively:
//!
//! * `time` becomes an RFC 3339 timestamp string whose fractional seconds
//!   carry the full nanosecond precision.
//! * `bytes` and arrays of `u8` become base64 strings. The two are identical
//!   on the wire, so they share one JSON rendering.
//! * `u64` and `i64` become canonical decimal strings, because JSON numbers
//!   lose precision above 2^53. The pattern pins the canonical form (no
//!   leading zeros, no `-0`); range is enforced when the value is parsed.
//! * `$optional` maps to an omitted `required` entry. `$optional` is legal
//!   only on root-level fields, so nested objects always require all of
//!   their properties.
//!
//! Public property names keep the DSL's snake_case spelling, not the
//! lowerCamelCase spelling the wire encoding uses internally. Every emitted
//! schema closes with `additionalProperties: false`.

use crate::error::Result;
use crate::generator::types::{
    validate_fixed_length_array_items, validate_message_format_field_names,
};
use config::node::{ArraySchema, MessageFormat, SchemaType, TypeToken};
use serde_json::{Map, Value, json};

/// Version of the mapping in this module. Recorded in every exposure bundle
/// so a bundle states which rules produced its schemas; the constant lives
/// with the bundle format so the runtime checks the same number it reads.
pub use peppy_mcp_catalog::SCHEMA_MAPPING_VERSION;

/// Canonical decimal rendering of a `u64`: no leading zeros.
const U64_DECIMAL_PATTERN: &str = "^(0|[1-9][0-9]*)$";
/// Canonical decimal rendering of an `i64`: no leading zeros, no `-0`.
const I64_DECIMAL_PATTERN: &str = "^(0|-?[1-9][0-9]*)$";

/// Derive the public JSON Schema for `format`, applying the same DSL rules
/// code generation enforces (fixed-length arrays hold scalars only, reserved
/// field names are refused) so a member that publishes is a member that
/// generates. `context` names the member in error messages.
pub fn message_format_to_json_schema(format: &MessageFormat, context: &str) -> Result<Value> {
    validate_message_format_field_names(format, context)?;
    validate_fixed_length_array_items(format)?;

    let mut properties = Map::new();
    let mut required = Vec::new();
    for (field_name, schema) in &format.0 {
        properties.insert(field_name.clone(), schema_type_to_json(schema));
        if !schema.is_optional() {
            required.push(Value::String(field_name.clone()));
        }
    }
    Ok(object_schema(properties, required))
}

/// The schema of a member with no declared payload: an object with no
/// properties.
pub fn empty_object_schema() -> Value {
    object_schema(Map::new(), Vec::new())
}

fn object_schema(properties: Map<String, Value>, required: Vec<Value>) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }
    schema.insert("additionalProperties".to_string(), json!(false));
    Value::Object(schema)
}

fn schema_type_to_json(schema: &SchemaType) -> Value {
    match schema {
        SchemaType::Type(token) => primitive_json(token),
        SchemaType::Primitive(primitive) => primitive_json(&primitive.kind),
        SchemaType::Array(array) => array_json(array),
        SchemaType::Object(object) => {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for (field_name, nested) in &object.fields {
                properties.insert(field_name.clone(), schema_type_to_json(nested));
                required.push(Value::String(field_name.clone()));
            }
            object_schema(properties, required)
        }
    }
}

fn array_json(array: &ArraySchema) -> Value {
    if array.items.as_ref().as_type_token() == Some(&TypeToken::U8) {
        return base64_string_json(array.length);
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("array"));
    schema.insert(
        "items".to_string(),
        schema_type_to_json(array.items.as_ref()),
    );
    if let Some(length) = array.length {
        schema.insert("minItems".to_string(), json!(length));
        schema.insert("maxItems".to_string(), json!(length));
    }
    Value::Object(schema)
}

/// `bytes` and arrays of `u8` share this rendering. A fixed byte length pins
/// the exact base64 text length (padding included).
fn base64_string_json(byte_length: Option<usize>) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("string"));
    schema.insert("contentEncoding".to_string(), json!("base64"));
    if let Some(bytes) = byte_length {
        let encoded = base64_encoded_len(bytes);
        schema.insert("minLength".to_string(), json!(encoded));
        schema.insert("maxLength".to_string(), json!(encoded));
    }
    Value::Object(schema)
}

fn base64_encoded_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

fn primitive_json(token: &TypeToken) -> Value {
    match token {
        TypeToken::Bool => json!({"type": "boolean"}),
        TypeToken::String => json!({"type": "string"}),
        TypeToken::Bytes => base64_string_json(None),
        TypeToken::Time => json!({"type": "string", "format": "date-time"}),
        TypeToken::U8 => bounded_integer_json(0, u8::MAX as i64),
        TypeToken::U16 => bounded_integer_json(0, u16::MAX as i64),
        TypeToken::U32 => bounded_integer_json(0, u32::MAX as i64),
        TypeToken::I8 => bounded_integer_json(i8::MIN as i64, i8::MAX as i64),
        TypeToken::I16 => bounded_integer_json(i16::MIN as i64, i16::MAX as i64),
        TypeToken::I32 => bounded_integer_json(i32::MIN as i64, i32::MAX as i64),
        TypeToken::U64 => json!({"type": "string", "pattern": U64_DECIMAL_PATTERN}),
        TypeToken::I64 => json!({"type": "string", "pattern": I64_DECIMAL_PATTERN}),
        TypeToken::F32 | TypeToken::F64 => json!({"type": "number"}),
    }
}

fn bounded_integer_json(minimum: i64, maximum: i64) -> Value {
    json!({"type": "integer", "minimum": minimum, "maximum": maximum})
}

/// Upper bound on the serialized JSON size of one value of `format`, under
/// the canonical mapping, or [`MaxSerializedSize::Unbounded`] when any
/// member (a string, `bytes`, or a variable-length array) has no static
/// maximum. Optional fields count as present. Publication uses this to
/// check `max_result_bytes` against members whose payload is finite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxSerializedSize {
    Bounded(u64),
    Unbounded,
}

pub fn max_serialized_json_bytes(format: &MessageFormat) -> MaxSerializedSize {
    match max_fields_bytes(format.0.iter()) {
        Some(bytes) => MaxSerializedSize::Bounded(bytes),
        None => MaxSerializedSize::Unbounded,
    }
}

/// Serialized size of `{...}` around named fields: braces, quoted names,
/// colons, commas, and each value's own maximum. `None` when any value is
/// unbounded.
fn max_fields_bytes<'a>(fields: impl Iterator<Item = (&'a String, &'a SchemaType)>) -> Option<u64> {
    let mut total: u64 = 2;
    let mut count: u64 = 0;
    for (field_name, schema) in fields {
        let value = max_value_bytes(schema)?;
        total = total
            .saturating_add(field_name.len() as u64 + 3)
            .saturating_add(value);
        count += 1;
    }
    Some(total.saturating_add(count.saturating_sub(1)))
}

fn max_value_bytes(schema: &SchemaType) -> Option<u64> {
    match schema {
        SchemaType::Type(token) => max_primitive_bytes(token),
        SchemaType::Primitive(primitive) => max_primitive_bytes(&primitive.kind),
        SchemaType::Object(object) => max_fields_bytes(object.fields.iter()),
        SchemaType::Array(array) => {
            let length = array.length? as u64;
            if array.items.as_ref().as_type_token() == Some(&TypeToken::U8) {
                // Quotes around the base64 text.
                return Some(2 + base64_encoded_len(length as usize) as u64);
            }
            let item = max_value_bytes(array.items.as_ref())?;
            // Brackets, items, and the commas between them.
            Some(
                2u64.saturating_add(item.saturating_mul(length))
                    .saturating_add(length.saturating_sub(1)),
            )
        }
    }
}

/// Worst-case serialized length of one primitive value, quotes included for
/// string renderings.
fn max_primitive_bytes(token: &TypeToken) -> Option<u64> {
    match token {
        // "false"
        TypeToken::Bool => Some(5),
        TypeToken::U8 => Some(3),
        TypeToken::U16 => Some(5),
        TypeToken::U32 => Some(10),
        TypeToken::I8 => Some(4),
        TypeToken::I16 => Some(6),
        TypeToken::I32 => Some(11),
        // 20 decimal digits (sign included for i64) plus quotes.
        TypeToken::U64 | TypeToken::I64 => Some(22),
        // Ryu shortest-representation worst cases.
        TypeToken::F32 => Some(16),
        TypeToken::F64 => Some(24),
        // Quotes, sign, a 12-digit proleptic year, `-MM-DDTHH:MM:SS`, nine
        // fractional digits with their dot, and `Z`.
        TypeToken::Time => Some(41),
        TypeToken::String | TypeToken::Bytes => None,
    }
}

#[cfg(test)]
mod tests;
