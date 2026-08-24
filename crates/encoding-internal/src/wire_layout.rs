//! The wire layout of a message format, as decided by the Cap'n Proto compiler.
//!
//! The schema renderer in the crate root fixes field names and ordinals. The
//! compiler then places every field inside its struct, as a data-section
//! offset or a pointer index, and sizes every struct. A [`StructLayout`]
//! records those decisions for one message format, field by field, so a codec
//! that walks it reads and writes exactly the bytes that accessors compiled
//! from the same schema do.
//!
//! Extraction pairs each message-format field with the compiled field of the
//! same ordinal and checks that the compiled name and type are the ones the
//! renderer maps that field to. A mismatch is reported rather than laid out:
//! the layout is only ever a faithful view of the compiler's output.

use crate::{Error, Result, capnp_type_name, sanitize_field_name};
use capnp::schema_capnp::{code_generator_request, field, node, type_};
use config::node::{ArraySchema, SchemaType, TypeToken};
use indexmap::IndexMap;
use std::collections::HashMap;

/// A struct's section sizes: the number of 64-bit words in its data section
/// and the number of pointers that follow them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructShape {
    pub data_words: u16,
    pub pointer_count: u16,
}

/// Where a field lives inside its struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSlot {
    /// A one-bit field at this bit offset into the data section.
    Bit(u32),
    /// A fixed-width scalar at this offset into the data section, counted in
    /// units of the scalar's own width.
    Data(u32),
    /// A pointer at this index into the pointer section.
    Pointer(u16),
}

/// A fixed-width value stored in a struct's data section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl Scalar {
    /// The scalar a token encodes as, or `None` for the pointer-backed tokens
    /// (`string`, `bytes`, `time`).
    pub fn from_token(token: &TypeToken) -> Option<Self> {
        match token {
            TypeToken::Bool => Some(Self::Bool),
            TypeToken::U8 => Some(Self::U8),
            TypeToken::U16 => Some(Self::U16),
            TypeToken::U32 => Some(Self::U32),
            TypeToken::U64 => Some(Self::U64),
            TypeToken::I8 => Some(Self::I8),
            TypeToken::I16 => Some(Self::I16),
            TypeToken::I32 => Some(Self::I32),
            TypeToken::I64 => Some(Self::I64),
            TypeToken::F32 => Some(Self::F32),
            TypeToken::F64 => Some(Self::F64),
            TypeToken::String | TypeToken::Bytes | TypeToken::Time => None,
        }
    }
}

/// The `Timestamp` struct a `time` field points to: `sec` is an `Int64` and
/// `nsec` a `UInt32`, each at its compiled data offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampLayout {
    pub shape: StructShape,
    pub sec: u32,
    pub nsec: u32,
}

/// What the elements of a list are.
#[derive(Debug, Clone, PartialEq)]
pub enum ListItems {
    Scalar(Scalar),
    Text,
    Bytes,
    Struct(StructLayout),
}

/// What a field holds.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Scalar(Scalar),
    Text,
    /// `bytes`, or an array of `u8`: both are one `Data` blob. A fixed array
    /// pins the byte count.
    Bytes {
        length: Option<usize>,
    },
    Time(TimestampLayout),
    Struct(StructLayout),
    /// A `List`. A fixed array pins the element count; the wire carries no
    /// length of its own.
    List {
        items: ListItems,
        length: Option<usize>,
    },
}

/// One field of a struct: its message-format name, whether the format marks
/// it `$optional`, where it lives and what it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldLayout {
    pub name: String,
    pub optional: bool,
    pub slot: FieldSlot,
    pub kind: FieldKind,
}

/// A struct and its fields, in message-format declaration order.
#[derive(Debug, Clone, PartialEq)]
pub struct StructLayout {
    pub shape: StructShape,
    pub fields: Vec<FieldLayout>,
}

/// The nodes of one compiled schema, by id, plus the id of the schema file.
pub(crate) struct SchemaNodes<'a> {
    by_id: HashMap<u64, node::Reader<'a>>,
    file_id: u64,
}

impl<'a> SchemaNodes<'a> {
    pub(crate) fn from_request(request: code_generator_request::Reader<'a>) -> Result<Self> {
        let requested_files = request.get_requested_files()?;
        if requested_files.is_empty() {
            return Err(Error::Encoding(
                "capnp request did not include any files".to_string(),
            ));
        }
        let file_id = requested_files.get(0).get_id();
        let by_id = request
            .get_nodes()?
            .iter()
            .map(|node| (node.get_id(), node))
            .collect();
        Ok(Self { by_id, file_id })
    }

    /// The id of the file-level `Message` struct every rendered schema roots at.
    pub(crate) fn root_message_id(&self) -> Result<u64> {
        for (id, node) in &self.by_id {
            if node.get_scope_id() != self.file_id {
                continue;
            }
            if !matches!(node.which(), Ok(node::Struct(_))) {
                continue;
            }
            let display_name = node
                .get_display_name()?
                .to_str()
                .map_err(|err| Error::Encoding(err.to_string()))?;
            let simple_name = display_name
                .rsplit(|ch| [':', '.'].contains(&ch))
                .next()
                .unwrap_or(display_name);
            if simple_name == "Message" {
                return Ok(*id);
            }
        }
        Err(Error::Encoding(
            "capnp request missing root Message struct".to_string(),
        ))
    }

    fn struct_node(&self, id: u64) -> Result<node::struct_::Reader<'a>> {
        let node = self.by_id.get(&id).ok_or_else(|| {
            Error::Encoding(format!(
                "capnp request missing node definition for struct id {id:#x}"
            ))
        })?;
        match node.which() {
            Ok(node::Struct(struct_reader)) => Ok(struct_reader),
            _ => Err(Error::Encoding(format!("node {id:#x} is not a struct"))),
        }
    }
}

/// Lays out the root `Message` struct of a compiled schema against the
/// message format it was rendered from.
pub(crate) fn extract(
    request: code_generator_request::Reader<'_>,
    fields: &IndexMap<String, SchemaType>,
) -> Result<StructLayout> {
    let nodes = SchemaNodes::from_request(request)?;
    let root = nodes.struct_node(nodes.root_message_id()?)?;
    layout_struct(&nodes, root, fields, "")
}

fn layout_struct(
    nodes: &SchemaNodes<'_>,
    compiled: node::struct_::Reader<'_>,
    fields: &IndexMap<String, SchemaType>,
    path: &str,
) -> Result<StructLayout> {
    let shape = StructShape {
        data_words: compiled.get_data_word_count(),
        pointer_count: compiled.get_pointer_count(),
    };
    let compiled_fields = compiled.get_fields()?;
    if compiled_fields.len() as usize != fields.len() {
        return Err(Error::Encoding(format!(
            "compiled struct `{path}` has {} fields, the message format declares {}",
            compiled_fields.len(),
            fields.len()
        )));
    }

    let mut laid_out = Vec::with_capacity(fields.len());
    for (index, (name, schema)) in fields.iter().enumerate() {
        let field_path = join_path(path, name);
        let compiled_field = compiled_fields.get(index as u32);
        let compiled_name = compiled_field
            .get_name()?
            .to_str()
            .map_err(|err| Error::Encoding(err.to_string()))?;
        let expected_name = sanitize_field_name(name);
        if compiled_name != expected_name {
            return Err(Error::Encoding(format!(
                "compiled field {index} of `{path}` is named `{compiled_name}`, expected `{expected_name}` for `{field_path}`"
            )));
        }
        let slot = match compiled_field.which().map_err(not_in_schema)? {
            field::Slot(slot) => slot,
            field::Group(_) => {
                return Err(Error::Encoding(format!(
                    "group fields are not supported for field `{field_path}`"
                )));
            }
        };
        let (slot, kind) = layout_field(
            nodes,
            &field_path,
            schema,
            slot.get_offset(),
            slot.get_type()?,
        )?;
        laid_out.push(FieldLayout {
            name: name.clone(),
            optional: schema.is_optional(),
            slot,
            kind,
        });
    }

    Ok(StructLayout {
        shape,
        fields: laid_out,
    })
}

fn layout_field(
    nodes: &SchemaNodes<'_>,
    path: &str,
    schema: &SchemaType,
    offset: u32,
    compiled: type_::Reader<'_>,
) -> Result<(FieldSlot, FieldKind)> {
    match schema {
        SchemaType::Type(token) => layout_token(nodes, path, token, offset, compiled),
        SchemaType::Primitive(primitive) => {
            layout_token(nodes, path, &primitive.kind, offset, compiled)
        }
        SchemaType::Array(array) => layout_array(nodes, path, array, offset, compiled),
        SchemaType::Object(object) => {
            let nested = expect_struct(nodes, path, "a nested object", compiled)?;
            let layout = layout_struct(nodes, nested, &object.fields, path)?;
            Ok((pointer_slot(path, offset)?, FieldKind::Struct(layout)))
        }
    }
}

fn layout_token(
    nodes: &SchemaNodes<'_>,
    path: &str,
    token: &TypeToken,
    offset: u32,
    compiled: type_::Reader<'_>,
) -> Result<(FieldSlot, FieldKind)> {
    if *token == TypeToken::Time {
        let timestamp = layout_timestamp(nodes, path, compiled)?;
        return Ok((pointer_slot(path, offset)?, FieldKind::Time(timestamp)));
    }
    expect_type(path, capnp_type_name(token), compiled)?;
    match token {
        TypeToken::Bool => Ok((FieldSlot::Bit(offset), FieldKind::Scalar(Scalar::Bool))),
        TypeToken::String => Ok((pointer_slot(path, offset)?, FieldKind::Text)),
        TypeToken::Bytes => Ok((
            pointer_slot(path, offset)?,
            FieldKind::Bytes { length: None },
        )),
        scalar => {
            let scalar = Scalar::from_token(scalar).ok_or_else(|| {
                Error::Encoding(format!("field `{path}` is not a fixed-width scalar"))
            })?;
            Ok((FieldSlot::Data(offset), FieldKind::Scalar(scalar)))
        }
    }
}

fn layout_array(
    nodes: &SchemaNodes<'_>,
    path: &str,
    array: &ArraySchema,
    offset: u32,
    compiled: type_::Reader<'_>,
) -> Result<(FieldSlot, FieldKind)> {
    let slot = pointer_slot(path, offset)?;
    if matches!(array.items.as_ref().as_type_token(), Some(TypeToken::U8)) {
        expect_type(path, "Data", compiled)?;
        return Ok((
            slot,
            FieldKind::Bytes {
                length: array.length,
            },
        ));
    }

    let element = match compiled.which().map_err(not_in_schema)? {
        type_::List(list) => list.get_element_type()?,
        other => {
            return Err(Error::Encoding(format!(
                "field `{path}` compiled as {} but the message format declares an array",
                type_name(other)
            )));
        }
    };
    let item_path = format!("{path}[]");
    let items = match array.items.as_ref() {
        SchemaType::Object(object) => {
            let nested = expect_struct(nodes, &item_path, "array items", element)?;
            ListItems::Struct(layout_struct(nodes, nested, &object.fields, &item_path)?)
        }
        SchemaType::Array(_) => {
            return Err(Error::Encoding(format!(
                "nested arrays are not supported for field `{path}`"
            )));
        }
        items => {
            let token = items
                .as_type_token()
                .expect("a non-array, non-object schema carries a type token");
            expect_type(&item_path, capnp_type_name(token), element)?;
            match token {
                TypeToken::String => ListItems::Text,
                TypeToken::Bytes => ListItems::Bytes,
                TypeToken::Time => {
                    return Err(Error::Encoding(format!(
                        "time arrays are not supported for field `{path}`"
                    )));
                }
                scalar => ListItems::Scalar(
                    Scalar::from_token(scalar)
                        .expect("every token other than string, bytes and time is a scalar"),
                ),
            }
        }
    };
    Ok((
        slot,
        FieldKind::List {
            items,
            length: array.length,
        },
    ))
}

fn layout_timestamp(
    nodes: &SchemaNodes<'_>,
    path: &str,
    compiled: type_::Reader<'_>,
) -> Result<TimestampLayout> {
    let timestamp = expect_struct(nodes, path, "a time", compiled)?;
    let shape = StructShape {
        data_words: timestamp.get_data_word_count(),
        pointer_count: timestamp.get_pointer_count(),
    };
    let mut sec = None;
    let mut nsec = None;
    for field in timestamp.get_fields()?.iter() {
        let name = field
            .get_name()?
            .to_str()
            .map_err(|err| Error::Encoding(err.to_string()))?;
        let field::Slot(slot) = field.which().map_err(not_in_schema)? else {
            return Err(Error::Encoding(format!(
                "Timestamp field `{name}` behind `{path}` is a group"
            )));
        };
        let field_path = format!("{path}.{name}");
        match name {
            "sec" => {
                expect_type(&field_path, "Int64", slot.get_type()?)?;
                sec = Some(slot.get_offset());
            }
            "nsec" => {
                expect_type(&field_path, "UInt32", slot.get_type()?)?;
                nsec = Some(slot.get_offset());
            }
            other => {
                return Err(Error::Encoding(format!(
                    "Timestamp behind `{path}` carries an unexpected field `{other}`"
                )));
            }
        }
    }
    match (sec, nsec) {
        (Some(sec), Some(nsec)) => Ok(TimestampLayout { shape, sec, nsec }),
        _ => Err(Error::Encoding(format!(
            "Timestamp behind `{path}` is missing its sec or nsec field"
        ))),
    }
}

fn expect_struct<'a>(
    nodes: &SchemaNodes<'a>,
    path: &str,
    what: &str,
    compiled: type_::Reader<'_>,
) -> Result<node::struct_::Reader<'a>> {
    match compiled.which().map_err(not_in_schema)? {
        type_::Struct(struct_type) => nodes.struct_node(struct_type.get_type_id()),
        other => Err(Error::Encoding(format!(
            "field `{path}` compiled as {} but the message format declares {what}",
            type_name(other)
        ))),
    }
}

fn expect_type(path: &str, expected: &str, compiled: type_::Reader<'_>) -> Result<()> {
    let actual = type_name(compiled.which().map_err(not_in_schema)?);
    if actual == expected {
        return Ok(());
    }
    Err(Error::Encoding(format!(
        "field `{path}` compiled as {actual} but the message format maps it to {expected}"
    )))
}

fn pointer_slot(path: &str, offset: u32) -> Result<FieldSlot> {
    u16::try_from(offset)
        .map(FieldSlot::Pointer)
        .map_err(|_| Error::Encoding(format!("pointer index {offset} of `{path}` exceeds u16")))
}

/// The schema spelling of a compiled type, matching [`capnp_type_name`] for
/// every type a message format produces other than the `Timestamp` struct,
/// which is checked structurally by [`layout_timestamp`].
fn type_name(compiled: type_::WhichReader<'_>) -> &'static str {
    match compiled {
        type_::Void(()) => "Void",
        type_::Bool(()) => "Bool",
        type_::Int8(()) => "Int8",
        type_::Int16(()) => "Int16",
        type_::Int32(()) => "Int32",
        type_::Int64(()) => "Int64",
        type_::Uint8(()) => "UInt8",
        type_::Uint16(()) => "UInt16",
        type_::Uint32(()) => "UInt32",
        type_::Uint64(()) => "UInt64",
        type_::Float32(()) => "Float32",
        type_::Float64(()) => "Float64",
        type_::Text(()) => "Text",
        type_::Data(()) => "Data",
        type_::List(_) => "List",
        type_::Enum(_) => "Enum",
        type_::Struct(_) => "Struct",
        type_::Interface(_) => "Interface",
        type_::AnyPointer(_) => "AnyPointer",
    }
}

fn not_in_schema(err: capnp::NotInSchema) -> Error {
    Error::Encoding(format!(
        "capnp request carries a value outside its schema: {err}"
    ))
}

fn join_path(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MessageFormatMapper;
    use config::node::MessageFormat;

    fn layout_of(format: &str) -> StructLayout {
        let format: MessageFormat = serde_json5::from_str(format).expect("valid format");
        MessageFormatMapper::new("layout_test", format)
            .wire_layout()
            .expect("the compiler lays the format out")
    }

    fn field<'a>(layout: &'a StructLayout, name: &str) -> &'a FieldLayout {
        layout
            .fields
            .iter()
            .find(|field| field.name == name)
            .unwrap_or_else(|| panic!("field `{name}` is laid out"))
    }

    #[test]
    fn scalars_are_packed_by_the_compiler_and_pointers_are_indexed_in_order() {
        let layout = layout_of(
            r#"{
                battery: "u8",
                temperature_c: "f32",
                frames_captured: "u64",
                recording: "bool",
                note: { $type: "string", $optional: true },
                calibrated_at: "time",
                checksum: { $type: "array", $items: "u8", $length: 4 },
                gains: { $type: "array", $items: "f32", $length: 3 },
                tags: { $type: "array", $items: "string" },
                pose: { $type: "object", x_m: "f64", y_m: "f64" },
                samples: { $type: "array", $items: { $type: "object", offset: "i16", value: "f64" } },
            }"#,
        );

        assert_eq!(
            layout.shape,
            StructShape {
                data_words: 2,
                pointer_count: 7
            }
        );
        assert_eq!(layout.fields.len(), 11, "one entry per declared field");
        assert_eq!(
            layout
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            [
                "battery",
                "temperature_c",
                "frames_captured",
                "recording",
                "note",
                "calibrated_at",
                "checksum",
                "gains",
                "tags",
                "pose",
                "samples"
            ],
            "fields keep declaration order"
        );

        // The compiler packs scalars by width: the u8 takes byte 0, the f32
        // the second 32-bit slot of word 0, the u64 word 1, the bool bit 8.
        assert_eq!(field(&layout, "battery").slot, FieldSlot::Data(0));
        assert_eq!(field(&layout, "temperature_c").slot, FieldSlot::Data(1));
        assert_eq!(field(&layout, "frames_captured").slot, FieldSlot::Data(1));
        assert_eq!(field(&layout, "recording").slot, FieldSlot::Bit(8));
        assert_eq!(
            field(&layout, "frames_captured").kind,
            FieldKind::Scalar(Scalar::U64)
        );

        let note = field(&layout, "note");
        assert!(note.optional);
        assert_eq!(note.slot, FieldSlot::Pointer(0));
        assert_eq!(note.kind, FieldKind::Text);

        let calibrated_at = field(&layout, "calibrated_at");
        assert_eq!(calibrated_at.slot, FieldSlot::Pointer(1));
        assert_eq!(
            calibrated_at.kind,
            FieldKind::Time(TimestampLayout {
                shape: StructShape {
                    data_words: 2,
                    pointer_count: 0
                },
                sec: 0,
                nsec: 2,
            })
        );

        assert_eq!(
            field(&layout, "checksum").kind,
            FieldKind::Bytes { length: Some(4) }
        );
        assert_eq!(
            field(&layout, "gains").kind,
            FieldKind::List {
                items: ListItems::Scalar(Scalar::F32),
                length: Some(3)
            }
        );
        assert_eq!(
            field(&layout, "tags").kind,
            FieldKind::List {
                items: ListItems::Text,
                length: None
            }
        );

        let FieldKind::Struct(pose) = &field(&layout, "pose").kind else {
            panic!("pose is a nested struct");
        };
        assert_eq!(
            pose.shape,
            StructShape {
                data_words: 2,
                pointer_count: 0
            }
        );
        assert_eq!(pose.fields[0].slot, FieldSlot::Data(0));
        assert_eq!(pose.fields[1].slot, FieldSlot::Data(1));

        let FieldKind::List {
            items: ListItems::Struct(sample),
            length: None,
        } = &field(&layout, "samples").kind
        else {
            panic!("samples is a list of structs");
        };
        assert_eq!(
            sample.shape,
            StructShape {
                data_words: 2,
                pointer_count: 0
            }
        );
        assert_eq!(sample.fields[0].kind, FieldKind::Scalar(Scalar::I16));
        assert_eq!(sample.fields[0].slot, FieldSlot::Data(0));
        assert_eq!(sample.fields[1].kind, FieldKind::Scalar(Scalar::F64));
        assert_eq!(sample.fields[1].slot, FieldSlot::Data(1));
    }

    #[test]
    fn a_u8_array_and_bytes_share_one_data_kind() {
        let layout = layout_of(
            r#"{ blob: "bytes", frame: { $type: "array", $items: "u8" }, pixels: { $type: "array", $items: "u8", $length: 3 } }"#,
        );
        assert_eq!(
            field(&layout, "blob").kind,
            FieldKind::Bytes { length: None }
        );
        assert_eq!(
            field(&layout, "frame").kind,
            FieldKind::Bytes { length: None }
        );
        assert_eq!(
            field(&layout, "pixels").kind,
            FieldKind::Bytes { length: Some(3) }
        );
        assert_eq!(
            layout.shape,
            StructShape {
                data_words: 0,
                pointer_count: 3
            }
        );
    }

    #[test]
    fn nested_struct_names_do_not_leak_into_the_layout() {
        let layout = layout_of(
            r#"{ profile: { $type: "object", gamma: "f64", white_balance: { $type: "object", red: "f32", blue: "f32" } } }"#,
        );
        let FieldKind::Struct(profile) = &field(&layout, "profile").kind else {
            panic!("profile is a struct");
        };
        let FieldKind::Struct(white_balance) = &field(profile, "white_balance").kind else {
            panic!("white_balance is a struct");
        };
        assert_eq!(white_balance.fields[0].name, "red");
        assert_eq!(white_balance.fields[0].slot, FieldSlot::Data(0));
        assert_eq!(white_balance.fields[1].slot, FieldSlot::Data(1));
        assert_eq!(
            white_balance.shape,
            StructShape {
                data_words: 1,
                pointer_count: 0
            }
        );
    }
}
