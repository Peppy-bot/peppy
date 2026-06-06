//! Lower-bound serialized-size estimate for a [`MessageFormat`].
//!
//! `stack benchmark` uses this to send service/action probes carrying a payload
//! of the message's real size, so the round-trip reflects real serialization +
//! transport instead of an empty sentinel. Fixed-shape messages size (closely)
//! exactly; variable-length fields — `string`, `bytes`, and unbounded arrays
//! (no `$length`) — cannot be known from the schema, so they contribute only
//! their fixed pointer overhead and flag the estimate as a lower bound.
//!
//! The numbers approximate Cap'n Proto's framed layout (the wire codec): a
//! word-aligned data section, an 8-byte pointer per variable/composite field,
//! and a small framing constant. Exactness is not important — the benchmark
//! sends that many opaque bytes, so only the order of magnitude (a few bytes vs
//! a few MB) matters.

use super::{ArraySchema, MessageFormat, ObjectSchema, PrimitiveSchema, SchemaType, TypeToken};

/// Estimated wire size of a message plus whether the schema contains
/// variable-length fields the estimate cannot account for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageSizeEstimate {
    /// Lower-bound serialized size in bytes.
    pub bytes: usize,
    /// `true` when a `string`/`bytes`/unbounded-array field is present, so a
    /// real message can be larger than `bytes`.
    pub has_variable: bool,
}

/// Cap'n Proto framing overhead added once per message (segment header).
const FRAMING_OVERHEAD: usize = 8;
/// A pointer-typed field (string/bytes/list/struct) costs a pointer word.
const POINTER_BYTES: usize = 8;

/// Lower-bound serialized size of `format`. See the module docs for the model.
pub fn estimate_serialized_size(format: &MessageFormat) -> MessageSizeEstimate {
    let mut acc = Acc::default();
    for schema in format.0.values() {
        size_of_schema(schema, &mut acc);
    }
    MessageSizeEstimate {
        bytes: round_up_8(acc.bytes) + FRAMING_OVERHEAD,
        has_variable: acc.has_variable,
    }
}

#[derive(Default)]
struct Acc {
    bytes: usize,
    has_variable: bool,
}

/// Byte width of a fixed-size primitive. `string`/`bytes` are 0 here and handled
/// as variable (pointer + flag) by the caller.
fn primitive_bytes(t: &TypeToken) -> usize {
    match t {
        TypeToken::Bool | TypeToken::U8 | TypeToken::I8 => 1,
        TypeToken::U16 | TypeToken::I16 => 2,
        TypeToken::U32 | TypeToken::I32 | TypeToken::F32 => 4,
        TypeToken::U64 | TypeToken::I64 | TypeToken::F64 | TypeToken::Time => 8,
        TypeToken::String | TypeToken::Bytes => 0,
    }
}

fn size_of_schema(schema: &SchemaType, acc: &mut Acc) {
    match schema {
        SchemaType::Type(t) | SchemaType::Primitive(PrimitiveSchema { kind: t, .. }) => match t {
            TypeToken::String | TypeToken::Bytes => {
                acc.bytes += POINTER_BYTES;
                acc.has_variable = true;
            }
            fixed => acc.bytes += primitive_bytes(fixed),
        },
        SchemaType::Array(ArraySchema { items, length, .. }) => {
            acc.bytes += POINTER_BYTES; // list pointer
            match length {
                Some(len) => {
                    let mut item = Acc::default();
                    size_of_schema(items, &mut item);
                    acc.bytes += len * item.bytes;
                    acc.has_variable |= item.has_variable;
                }
                // Unbounded array: the element count is a runtime property.
                None => acc.has_variable = true,
            }
        }
        SchemaType::Object(ObjectSchema { fields, .. }) => {
            acc.bytes += POINTER_BYTES; // struct pointer
            let mut inner = Acc::default();
            for field in fields.values() {
                size_of_schema(field, &mut inner);
            }
            acc.bytes += round_up_8(inner.bytes);
            acc.has_variable |= inner.has_variable;
        }
    }
}

fn round_up_8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeConfigParser;

    /// Parse a node whose one exposed service carries `req`/`resp` formats, and
    /// return the parsed request/response `MessageFormat`s.
    fn service_formats(req: &str, resp: &str) -> (MessageFormat, MessageFormat) {
        let cfg = NodeConfigParser::from_content(&format!(
            r#"{{
                peppy_schema: "node_v1",
                manifest: {{ name: "n", tag: "v1" }},
                execution: {{ language: "rust", run_cmd: ["n"] }},
                interfaces: {{ services: {{ exposes: [ {{
                    name: "svc",
                    request_message_format: {req},
                    response_message_format: {resp}
                }} ] }} }}
            }}"#
        ))
        .expect("parse");
        let svc = cfg
            .interfaces
            .services
            .unwrap()
            .exposes
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        (
            svc.request_message_format.unwrap(),
            svc.response_message_format.unwrap(),
        )
    }

    #[test]
    fn fixed_primitives_are_sized_exactly_and_not_variable() {
        let (req, _) = service_formats(r#"{ a: "u32", b: "f64", c: "bool" }"#, "{}");
        let est = estimate_serialized_size(&req);
        // 4 + 8 + 1 = 13 → round to 16 + 8 framing = 24; nothing variable.
        assert_eq!(est.bytes, 24);
        assert!(!est.has_variable);
    }

    #[test]
    fn empty_message_is_just_framing() {
        let (_, resp) = service_formats("{}", "{}");
        let est = estimate_serialized_size(&resp);
        assert_eq!(est.bytes, FRAMING_OVERHEAD);
        assert!(!est.has_variable);
    }

    #[test]
    fn fixed_length_array_is_counted_exactly() {
        let (req, _) = service_formats(
            r#"{ joints: { $type: "array", $items: "f64", $length: 6 } }"#,
            "{}",
        );
        let est = estimate_serialized_size(&req);
        // pointer(8) + 6*8 = 56 → already a multiple of 8 → +8 framing = 64.
        assert_eq!(est.bytes, 64);
        assert!(!est.has_variable);
    }

    #[test]
    fn strings_and_unbounded_arrays_flag_variable_as_lower_bound() {
        let (req, _) = service_formats(
            r#"{ name: "string", frame: { $type: "array", $items: "u8" } }"#,
            "{}",
        );
        let est = estimate_serialized_size(&req);
        assert!(
            est.has_variable,
            "string + unbounded array must flag variable"
        );
        // Only the two pointer words are counted (no content known): 16 → +8 = 24.
        assert_eq!(est.bytes, 24);
    }
}
