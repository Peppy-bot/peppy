//! The codec: canonical JSON to wire bytes and back, walking a wire layout.
//!
//! Encoding visits the layout's fields in declaration order and allocates
//! every pointer target (text, data, nested struct, list) at the point of
//! its field, into a default heap allocator, then frames the message with
//! [`capnp::serialize::write_message`]. That is the sequence generated
//! nodes follow, so the two produce the same bytes for the same values.
//! Decoding reads each field back through the same accessors, in place in
//! the framed bytes as the transport handed them over (no alignment is
//! assumed of them), and renders it with the canonical value rules.
//!
//! An optional field is present exactly when its pointer is non-null; it is
//! omitted from the JSON object when absent and never written as `null`.

use crate::dynamic::{RootBuilder, RootReader, init_struct_list, read_struct_list, struct_size};
use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::private::layout::{
    ElementSize, PointerBuilder, PointerReader, PrimitiveElement, StructBuilder, StructReader,
};
use capnp::traits::{FromPointerBuilder, FromPointerReader};
use capnp::{data_list, primitive_list, serialize, text, text_list};
use config::node::MessageFormat;
use encoding::MessageFormatMapper;
use encoding::wire_layout::{
    FieldKind, FieldLayout, ListItems, PointerTarget, Scalar, StructLayout, TimestampLayout,
};
use peppy_mcp_runtime::bridge;
use peppylib::encoding::{CapnpTimestamp, convert_time, convert_time_from_capnp};
use serde_json::{Map, Value};

/// Building a codec failed: the message format could not be laid out.
#[derive(Debug, thiserror::Error)]
#[error("cannot lay out `{schema_name}` on the wire: {source}")]
pub struct CodecError {
    schema_name: String,
    #[source]
    source: Box<config::ConfigError>,
}

/// A value did not convert. The message names the offending field the way
/// generated bridges do, so it can be surfaced to a client verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConversionError(String);

impl From<ConversionError> for String {
    fn from(error: ConversionError) -> Self {
        error.0
    }
}

impl From<String> for ConversionError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<capnp::Error> for ConversionError {
    fn from(error: capnp::Error) -> Self {
        Self(error.to_string())
    }
}

/// Converts between canonical JSON and the wire encoding of one message
/// format.
#[derive(Debug, Clone)]
pub struct MessageCodec {
    layout: StructLayout,
}

impl MessageCodec {
    /// Lays `format` out through the bundled Cap'n Proto compiler.
    /// `schema_name` seeds the schema's file id, as it does for code
    /// generation; the id does not appear in messages.
    pub fn new(schema_name: &str, format: MessageFormat) -> Result<Self, CodecError> {
        let layout = MessageFormatMapper::new(schema_name, format)
            .wire_layout()
            .map_err(|source| CodecError {
                schema_name: schema_name.to_string(),
                source: Box::new(source),
            })?;
        Ok(Self::from_layout(layout))
    }

    pub fn from_layout(layout: StructLayout) -> Self {
        Self { layout }
    }

    pub fn layout(&self) -> &StructLayout {
        &self.layout
    }

    /// Encodes a JSON object into a framed message.
    pub fn encode(&self, value: &Value) -> Result<Vec<u8>, ConversionError> {
        let mut message = Builder::new_default();
        {
            let root = message
                .init_root::<RootBuilder<'_>>()
                .0
                .init_struct(struct_size(self.layout.shape));
            encode_struct(root, &self.layout, value)?;
        }
        Ok(frame(&message))
    }

    /// Decodes a framed message into a JSON object, reading it in place:
    /// nothing is copied out of `bytes` before its values are converted.
    pub fn decode(&self, mut bytes: &[u8]) -> Result<Value, ConversionError> {
        let message = serialize::read_message_from_flat_slice(&mut bytes, ReaderOptions::new())?;
        let root = message.get_root::<RootReader<'_>>()?.0.get_struct(None)?;
        decode_struct(root, &self.layout)
    }
}

/// Frames the message into one buffer sized from the message up front, so
/// a large frame is written once rather than grown into.
fn frame(message: &Builder<HeapAllocator>) -> Vec<u8> {
    serialize::write_message_to_words(message)
}

fn encode_struct(
    mut builder: StructBuilder<'_>,
    layout: &StructLayout,
    value: &Value,
) -> Result<(), ConversionError> {
    for field in &layout.fields {
        let field_value = if field.optional {
            match bridge::optional(value, &field.name) {
                Some(field_value) => field_value,
                None => continue,
            }
        } else {
            bridge::require(value, &field.name)?
        };
        encode_field(builder.reborrow(), field, field_value)?;
    }
    Ok(())
}

fn encode_field(
    builder: StructBuilder<'_>,
    field: &FieldLayout,
    value: &Value,
) -> Result<(), ConversionError> {
    let name = field.name.as_str();
    match &field.kind {
        FieldKind::Bit(bit) => {
            builder.set_bool_field(*bit as usize, bridge::value_bool(value, name)?);
            Ok(())
        }
        FieldKind::Data { offset, scalar } => {
            encode_scalar(&builder, *offset as usize, *scalar, name, value)
        }
        FieldKind::Pointer { index, target } => {
            let pointer = builder.get_pointer_field(*index as usize);
            encode_pointer(pointer, target, name, value)
        }
    }
}

fn encode_scalar(
    builder: &StructBuilder<'_>,
    offset: usize,
    scalar: Scalar,
    name: &str,
    value: &Value,
) -> Result<(), ConversionError> {
    match scalar {
        Scalar::U8 => builder.set_data_field::<u8>(offset, bridge::value_u8(value, name)?),
        Scalar::U16 => builder.set_data_field::<u16>(offset, bridge::value_u16(value, name)?),
        Scalar::U32 => builder.set_data_field::<u32>(offset, bridge::value_u32(value, name)?),
        Scalar::U64 => {
            builder.set_data_field::<u64>(offset, bridge::value_u64_decimal(value, name)?)
        }
        Scalar::I8 => builder.set_data_field::<i8>(offset, bridge::value_i8(value, name)?),
        Scalar::I16 => builder.set_data_field::<i16>(offset, bridge::value_i16(value, name)?),
        Scalar::I32 => builder.set_data_field::<i32>(offset, bridge::value_i32(value, name)?),
        Scalar::I64 => {
            builder.set_data_field::<i64>(offset, bridge::value_i64_decimal(value, name)?)
        }
        Scalar::F32 => builder.set_data_field::<f32>(offset, value_f32(value, name)?),
        Scalar::F64 => builder.set_data_field::<f64>(offset, bridge::value_f64(value, name)?),
    }
    Ok(())
}

/// JSON carries one float width, so an `f32` arrives as an `f64` and is
/// narrowed here. `as` saturates to an infinity, which the schema's
/// `{"type": "number"}` cannot exclude, so the range is checked instead of
/// letting an out-of-range value become `inf` on the wire.
fn value_f32(value: &Value, name: &str) -> Result<f32, ConversionError> {
    let wide = bridge::value_f64(value, name)?;
    if !wide.is_finite() || wide < f32::MIN as f64 || wide > f32::MAX as f64 {
        return Err(ConversionError(format!(
            "`{name}` is outside the range of a 32-bit float"
        )));
    }
    Ok(wide as f32)
}

fn encode_pointer(
    mut pointer: PointerBuilder<'_>,
    target: &PointerTarget,
    name: &str,
    value: &Value,
) -> Result<(), ConversionError> {
    match target {
        PointerTarget::Text => {
            pointer.set_text(text::Reader::from(bridge::value_str(value, name)?));
        }
        PointerTarget::Bytes { length } => {
            let bytes = bridge::value_bytes(value, name)?;
            if let Some(length) = length
                && bytes.len() != *length
            {
                return Err(ConversionError(format!(
                    "`{name}` must decode to exactly {length} bytes"
                )));
            }
            pointer.set_data(&bytes);
        }
        PointerTarget::Time(timestamp) => {
            let time = convert_time(bridge::value_time(value, name)?);
            let builder = pointer.init_struct(struct_size(timestamp.shape));
            builder.set_data_field::<i64>(timestamp.sec as usize, time.sec);
            builder.set_data_field::<u32>(timestamp.nsec as usize, time.nsec);
        }
        PointerTarget::Struct(layout) => {
            let builder = pointer.init_struct(struct_size(layout.shape));
            encode_struct(builder, layout, value)?;
        }
        PointerTarget::List { items, length } => {
            let elements = bridge::value_array(value, name)?;
            if let Some(length) = length
                && elements.len() != *length
            {
                return Err(ConversionError(format!(
                    "`{name}` must have exactly {length} items"
                )));
            }
            let count = u32::try_from(elements.len()).map_err(|_| {
                ConversionError(format!("`{name}` has more items than a list holds"))
            })?;
            encode_list(pointer, items, name, elements, count)?;
        }
    }
    Ok(())
}

fn encode_list(
    pointer: PointerBuilder<'_>,
    items: &ListItems,
    name: &str,
    elements: &[Value],
    count: u32,
) -> Result<(), ConversionError> {
    match items {
        ListItems::Bool => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_bool(item, name)
        }),
        ListItems::Scalar(Scalar::U8) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_u8(item, name)
        }),
        ListItems::Scalar(Scalar::U16) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_u16(item, name)
        }),
        ListItems::Scalar(Scalar::U32) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_u32(item, name)
        }),
        ListItems::Scalar(Scalar::U64) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_u64_decimal(item, name)
        }),
        ListItems::Scalar(Scalar::I8) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_i8(item, name)
        }),
        ListItems::Scalar(Scalar::I16) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_i16(item, name)
        }),
        ListItems::Scalar(Scalar::I32) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_i32(item, name)
        }),
        ListItems::Scalar(Scalar::I64) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_i64_decimal(item, name)
        }),
        ListItems::Scalar(Scalar::F32) => {
            encode_scalar_list(pointer, count, elements, |item| value_f32(item, name))
        }
        ListItems::Scalar(Scalar::F64) => encode_scalar_list(pointer, count, elements, |item| {
            bridge::value_f64(item, name)
        }),
        ListItems::Text => {
            let mut list = text_list::Builder::init_pointer(pointer, count);
            for (index, item) in elements.iter().enumerate() {
                list.set(index as u32, bridge::value_str(item, name)?);
            }
            Ok(())
        }
        ListItems::Bytes => {
            let mut list = data_list::Builder::init_pointer(pointer, count);
            for (index, item) in elements.iter().enumerate() {
                list.set(index as u32, &bridge::value_bytes(item, name)?);
            }
            Ok(())
        }
        ListItems::Struct(layout) => {
            let mut list = init_struct_list(pointer, count, layout.shape)?;
            for (index, item) in elements.iter().enumerate() {
                let element = list.reborrow().get(index as u32).0;
                encode_struct(element, layout, item)?;
            }
            Ok(())
        }
    }
}

fn encode_scalar_list<T, E>(
    pointer: PointerBuilder<'_>,
    count: u32,
    elements: &[Value],
    convert: impl Fn(&Value) -> Result<T, E>,
) -> Result<(), ConversionError>
where
    T: PrimitiveElement,
    ConversionError: From<E>,
{
    let mut list = primitive_list::Builder::<T>::init_pointer(pointer, count);
    for (index, item) in elements.iter().enumerate() {
        list.set(index as u32, convert(item)?);
    }
    Ok(())
}

fn decode_struct(
    reader: StructReader<'_>,
    layout: &StructLayout,
) -> Result<Value, ConversionError> {
    let mut object = Map::new();
    for field in &layout.fields {
        if field.optional && is_absent(&reader, &field.kind) {
            continue;
        }
        object.insert(field.name.clone(), decode_field(&reader, field)?);
    }
    Ok(Value::Object(object))
}

/// An optional field is absent when its pointer is null. Only pointer-backed
/// fields can be optional, so any other kind reads as present.
fn is_absent(reader: &StructReader<'_>, kind: &FieldKind) -> bool {
    matches!(kind, FieldKind::Pointer { index, .. } if reader.is_pointer_field_null(*index as usize))
}

fn decode_field(reader: &StructReader<'_>, field: &FieldLayout) -> Result<Value, ConversionError> {
    match &field.kind {
        FieldKind::Bit(bit) => Ok(Value::from(reader.get_bool_field(*bit as usize))),
        FieldKind::Data { offset, scalar } => decode_scalar(reader, *offset as usize, *scalar),
        FieldKind::Pointer { index, target } => decode_pointer(
            reader.get_pointer_field(*index as usize),
            target,
            &field.name,
        ),
    }
}

fn decode_scalar(
    reader: &StructReader<'_>,
    offset: usize,
    scalar: Scalar,
) -> Result<Value, ConversionError> {
    let value = match scalar {
        Scalar::U8 => Value::from(reader.get_data_field::<u8>(offset)),
        Scalar::U16 => Value::from(reader.get_data_field::<u16>(offset)),
        Scalar::U32 => Value::from(reader.get_data_field::<u32>(offset)),
        Scalar::U64 => Value::String(reader.get_data_field::<u64>(offset).to_string()),
        Scalar::I8 => Value::from(reader.get_data_field::<i8>(offset)),
        Scalar::I16 => Value::from(reader.get_data_field::<i16>(offset)),
        Scalar::I32 => Value::from(reader.get_data_field::<i32>(offset)),
        Scalar::I64 => Value::String(reader.get_data_field::<i64>(offset).to_string()),
        Scalar::F32 => bridge::float_to_json(f64::from(reader.get_data_field::<f32>(offset)))?,
        Scalar::F64 => bridge::float_to_json(reader.get_data_field::<f64>(offset))?,
    };
    Ok(value)
}

fn decode_pointer(
    pointer: PointerReader<'_>,
    target: &PointerTarget,
    name: &str,
) -> Result<Value, ConversionError> {
    match target {
        PointerTarget::Text => Ok(Value::String(decode_text(pointer.get_text(None)?, name)?)),
        PointerTarget::Bytes { length } => {
            let bytes = pointer.get_data(None)?;
            if let Some(length) = length
                && bytes.len() != *length
            {
                return Err(ConversionError(format!(
                    "invalid fixed bytes length for field '{name}': expected {length}, got {}",
                    bytes.len()
                )));
            }
            Ok(Value::String(bridge::bytes_to_base64(bytes)))
        }
        PointerTarget::Time(timestamp) => {
            let time = decode_time(pointer, timestamp)?;
            Ok(Value::String(bridge::time_to_rfc3339(time)))
        }
        PointerTarget::Struct(layout) => decode_struct(pointer.get_struct(None)?, layout),
        PointerTarget::List { items, length } => {
            let elements = decode_list(pointer, items, name)?;
            if let Some(length) = length
                && elements.len() != *length
            {
                return Err(ConversionError(format!(
                    "invalid fixed list length for field '{name}': expected {length}, got {}",
                    elements.len()
                )));
            }
            Ok(Value::Array(elements))
        }
    }
}

fn decode_time(
    pointer: PointerReader<'_>,
    timestamp: &TimestampLayout,
) -> Result<std::time::SystemTime, ConversionError> {
    let reader = pointer.get_struct(None)?;
    Ok(convert_time_from_capnp(CapnpTimestamp {
        sec: reader.get_data_field::<i64>(timestamp.sec as usize),
        nsec: reader.get_data_field::<u32>(timestamp.nsec as usize),
    }))
}

fn decode_text(text: text::Reader<'_>, name: &str) -> Result<String, ConversionError> {
    text.to_string()
        .map_err(|error| ConversionError(format!("field '{name}' is not UTF-8: {error}")))
}

fn decode_list(
    pointer: PointerReader<'_>,
    items: &ListItems,
    name: &str,
) -> Result<Vec<Value>, ConversionError> {
    match items {
        ListItems::Bool => decode_scalar_list::<bool>(pointer, Value::from),
        ListItems::Scalar(Scalar::U8) => decode_scalar_list::<u8>(pointer, Value::from),
        ListItems::Scalar(Scalar::U16) => decode_scalar_list::<u16>(pointer, Value::from),
        ListItems::Scalar(Scalar::U32) => decode_scalar_list::<u32>(pointer, Value::from),
        ListItems::Scalar(Scalar::U64) => {
            decode_scalar_list::<u64>(pointer, |item| Value::String(item.to_string()))
        }
        ListItems::Scalar(Scalar::I8) => decode_scalar_list::<i8>(pointer, Value::from),
        ListItems::Scalar(Scalar::I16) => decode_scalar_list::<i16>(pointer, Value::from),
        ListItems::Scalar(Scalar::I32) => decode_scalar_list::<i32>(pointer, Value::from),
        ListItems::Scalar(Scalar::I64) => {
            decode_scalar_list::<i64>(pointer, |item| Value::String(item.to_string()))
        }
        ListItems::Scalar(Scalar::F32) => {
            let list = primitive_list::Reader::<f32>::get_from_pointer(&pointer, None)?;
            list.iter()
                .map(|item| bridge::float_to_json(f64::from(item)).map_err(ConversionError))
                .collect()
        }
        ListItems::Scalar(Scalar::F64) => {
            let list = primitive_list::Reader::<f64>::get_from_pointer(&pointer, None)?;
            list.iter()
                .map(|item| bridge::float_to_json(item).map_err(ConversionError))
                .collect()
        }
        ListItems::Text => {
            let list = text_list::Reader::new(pointer.get_list(ElementSize::Pointer, None)?);
            list.iter()
                .map(|item| Ok(Value::String(decode_text(item?, name)?)))
                .collect()
        }
        ListItems::Bytes => {
            let list = data_list::Reader::new(pointer.get_list(ElementSize::Pointer, None)?);
            list.iter()
                .map(|item| Ok(Value::String(bridge::bytes_to_base64(item?))))
                .collect()
        }
        ListItems::Struct(layout) => {
            let list = read_struct_list(pointer)?;
            (0..list.len())
                .map(|index| decode_struct(list.get(index).0, layout))
                .collect()
        }
    }
}

fn decode_scalar_list<T>(
    pointer: PointerReader<'_>,
    render: impl Fn(T) -> Value,
) -> Result<Vec<Value>, ConversionError>
where
    T: PrimitiveElement,
{
    let list = primitive_list::Reader::<T>::get_from_pointer(&pointer, None)?;
    Ok(list.iter().map(render).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn codec(format: &str) -> MessageCodec {
        let format: MessageFormat = serde_json5::from_str(format).expect("valid format");
        MessageCodec::new("codec_test", format).expect("the format lays out")
    }

    fn round_trip(codec: &MessageCodec, value: Value) -> Vec<u8> {
        let bytes = codec.encode(&value).expect("encodes");
        let decoded = codec.decode(&bytes).expect("decodes");
        assert_eq!(decoded, value, "decode(encode(v)) == v");
        let again = codec.encode(&decoded).expect("re-encodes");
        assert_eq!(again, bytes, "encode(decode(bytes)) == bytes");
        bytes
    }

    #[test]
    fn every_scalar_round_trips_with_its_canonical_rendering() {
        let codec = codec(
            r#"{
                flag: "bool", tiny: "u8", small: "u16", medium: "u32", big: "u64",
                tiny_signed: "i8", small_signed: "i16", medium_signed: "i32", big_signed: "i64",
                ratio: "f32", precise: "f64",
            }"#,
        );
        round_trip(
            &codec,
            json!({
                "flag": true, "tiny": 255, "small": 65535, "medium": 4294967295u32,
                "big": "18446744073709551615",
                "tiny_signed": -128, "small_signed": -32768, "medium_signed": -2147483648i32,
                "big_signed": "-9223372036854775808",
                "ratio": 1.5, "precise": -2.25,
            }),
        );
    }

    #[test]
    fn a_scalar_message_has_the_layout_the_compiler_assigned() {
        // Two words of data, no pointers: `a` at byte 0, `b` in the second
        // 32-bit slot of word 0, `c` in word 1, and the bool at bit 8.
        let codec = codec(r#"{ a: "u8", b: "f32", c: "u64", d: "bool" }"#);
        let bytes = codec
            .encode(&json!({ "a": 7, "b": 1.0, "c": "258", "d": true }))
            .expect("encodes");
        let expected: Vec<u8> = [
            // Segment table: the segment count less one, then 3 words.
            &[0, 0, 0, 0, 3, 0, 0, 0][..],
            // Root pointer: struct at offset 0, 2 data words, 0 pointers.
            &[0, 0, 0, 0, 2, 0, 0, 0],
            // Word 0: a=7, bit 8 set (d), f32 1.0 in bytes 4..8.
            &[7, 1, 0, 0, 0, 0, 0x80, 0x3f],
            // Word 1: c = 258 little-endian.
            &[2, 1, 0, 0, 0, 0, 0, 0],
        ]
        .concat();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn pointer_backed_fields_round_trip_and_optional_ones_are_omitted_when_absent() {
        let codec = codec(
            r#"{
                label: "string",
                blob: "bytes",
                stamp: "time",
                note: { $type: "string", $optional: true },
                extra: { $type: "bytes", $optional: true },
                seen: { $type: "time", $optional: true },
            }"#,
        );
        let with_all = json!({
            "label": "camera",
            "blob": "AAH+/w==",
            "stamp": "2023-11-14T22:13:20.123456789Z",
            "note": "",
            "extra": "",
            "seen": "1970-01-01T00:00:00.000000000Z",
        });
        let full = round_trip(&codec, with_all);
        let without = round_trip(
            &codec,
            json!({
                "label": "camera",
                "blob": "AAH+/w==",
                "stamp": "2023-11-14T22:13:20.123456789Z",
            }),
        );
        assert!(
            without.len() < full.len(),
            "absent optionals leave their pointers null and allocate nothing"
        );
        let decoded = codec.decode(&without).expect("decodes");
        assert!(
            decoded.get("note").is_none(),
            "absent stays absent, never null"
        );
    }

    #[test]
    fn nested_objects_lists_and_fixed_arrays_round_trip() {
        let codec = codec(
            r#"{
                checksum: { $type: "array", $items: "u8", $length: 4 },
                gains: { $type: "array", $items: "f32", $length: 3 },
                flags: { $type: "array", $items: "bool" },
                tags: { $type: "array", $items: "string" },
                chunks: { $type: "array", $items: "bytes" },
                counts: { $type: "array", $items: "u64" },
                pose: { $type: "object", x_m: "f64", y_m: "f64" },
                samples: { $type: "array", $items: { $type: "object", offset: "i16", label: "string" } },
                profile: { $type: "object", gamma: "f64", white_balance: { $type: "object", red: "f32", blue: "f32" } },
                empty: { $type: "array", $items: "i32" },
                maybe_pose: { $type: "object", $optional: true, x_m: "f64" },
                maybe_list: { $type: "array", $optional: true, $items: "u16" },
            }"#,
        );
        round_trip(
            &codec,
            json!({
                "checksum": "AQIDBA==",
                "gains": [0.5, 1.0, 1.5],
                "flags": [true, false, true, true, false, false, false, true, true],
                "tags": ["a", "", "ccc"],
                "chunks": ["AQ==", ""],
                "counts": ["0", "18446744073709551615"],
                "pose": { "x_m": 1.25, "y_m": -3.5 },
                "samples": [
                    { "offset": -1, "label": "first" },
                    { "offset": 32767, "label": "" },
                ],
                "profile": { "gamma": 2.2, "white_balance": { "red": 0.75, "blue": 1.25 } },
                "empty": [],
                "maybe_pose": { "x_m": 0.0 },
                "maybe_list": [1, 2, 3],
            }),
        );
        round_trip(
            &codec,
            json!({
                "checksum": "AQIDBA==",
                "gains": [0.5, 1.0, 1.5],
                "flags": [],
                "tags": [],
                "chunks": [],
                "counts": [],
                "pose": { "x_m": 0.0, "y_m": 0.0 },
                "samples": [],
                "profile": { "gamma": 0.0, "white_balance": { "red": 0.0, "blue": 0.0 } },
                "empty": [],
            }),
        );
    }

    #[test]
    fn encoding_refuses_values_outside_the_canonical_mapping() {
        let codec = codec(
            r#"{
                tiny: "u8",
                big: "u64",
                ratio: "f32",
                stamp: "time",
                blob: "bytes",
                checksum: { $type: "array", $items: "u8", $length: 4 },
                gains: { $type: "array", $items: "f32", $length: 3 },
                note: { $type: "string", $optional: true },
            }"#,
        );
        let valid = json!({
            "tiny": 1, "big": "5", "ratio": 1.0, "stamp": "2023-11-14T22:13:20Z",
            "blob": "AQ==", "checksum": "AQIDBA==", "gains": [1.0, 2.0, 3.0],
        });
        codec.encode(&valid).expect("the valid message encodes");

        let refused = |patch: fn(&mut Map<String, Value>), message: &str| {
            let mut value = valid.clone();
            patch(value.as_object_mut().expect("object"));
            let error = codec.encode(&value).expect_err("refused");
            assert_eq!(error.0, message, "for {value}");
        };
        refused(
            |v| {
                v.remove("tiny");
            },
            "`tiny` is missing",
        );
        refused(
            |v| {
                v.insert("tiny".into(), json!(256));
            },
            "`tiny` is not an integer between 0 and 255",
        );
        refused(
            |v| {
                v.insert("big".into(), json!(5));
            },
            "`big` is not a decimal string",
        );
        refused(
            |v| {
                v.insert("big".into(), json!("05"));
            },
            "`big` is not a canonical decimal string",
        );
        refused(
            |v| {
                v.insert("ratio".into(), json!(1e40));
            },
            "`ratio` is outside the range of a 32-bit float",
        );
        refused(
            |v| {
                v.insert("stamp".into(), json!("2023-11-14T22:13:20+00:00"));
            },
            "`stamp` is not an RFC 3339 UTC timestamp (`...Z`)",
        );
        refused(
            |v| {
                v.insert("blob".into(), json!("not base64!"));
            },
            "`blob` is not valid base64",
        );
        refused(
            |v| {
                v.insert("checksum".into(), json!("AQID"));
            },
            "`checksum` must decode to exactly 4 bytes",
        );
        refused(
            |v| {
                v.insert("gains".into(), json!([1.0, 2.0]));
            },
            "`gains` must have exactly 3 items",
        );
        refused(
            |v| {
                v.insert("note".into(), Value::Null);
            },
            "`note` is not a string",
        );
    }

    #[test]
    fn decoding_refuses_a_fixed_length_mismatch_and_non_utf8_text() {
        let variable = codec(r#"{ pixels: { $type: "array", $items: "u8" }, name: "string" }"#);
        let fixed =
            codec(r#"{ pixels: { $type: "array", $items: "u8", $length: 2 }, name: "string" }"#);
        let bytes = variable
            .encode(&json!({ "pixels": "AQID", "name": "x" }))
            .expect("encodes");
        let error = fixed
            .decode(&bytes)
            .expect_err("three bytes do not fit two");
        assert_eq!(
            error.0,
            "invalid fixed bytes length for field 'pixels': expected 2, got 3"
        );

        // The same layout with `name` declared as bytes writes a raw 0xff
        // followed by the NUL a text carries, which the text reader must
        // refuse as UTF-8.
        let raw = codec(r#"{ pixels: { $type: "array", $items: "u8" }, name: "bytes" }"#);
        let bytes = raw
            .encode(&json!({ "pixels": "AQID", "name": "/wA=" }))
            .expect("encodes");
        let error = variable.decode(&bytes).expect_err("0xff is not UTF-8");
        assert!(error.0.starts_with("field 'name' is not UTF-8"), "{error}");
    }

    #[test]
    fn a_message_encoded_without_optional_fields_reads_defaults_for_required_ones() {
        // A producer built against a schema that ends earlier (or left a
        // pointer unset) yields the Cap'n Proto defaults, exactly as
        // generated readers do: empty text, empty data, empty list.
        let short = codec(r#"{ count: "u32" }"#);
        let long = codec(
            r#"{ count: "u32", label: "string", tags: { $type: "array", $items: "string" } }"#,
        );
        let bytes = short.encode(&json!({ "count": 3 })).expect("encodes");
        assert_eq!(
            long.decode(&bytes).expect("decodes"),
            json!({ "count": 3, "label": "", "tags": [] })
        );
    }

    #[test]
    fn decoding_refuses_bytes_that_are_not_a_framed_message() {
        let codec = codec(r#"{ count: "u32" }"#);
        assert!(codec.decode(&[1, 2, 3]).is_err());
    }
}
