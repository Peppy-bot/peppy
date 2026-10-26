//! The codec API of peppy: calls to contracts that a node was not built
//! with.
//!
//! A node built with peppy calls the members of the contracts that its
//! manifest names through generated code. This crate serves the calls to
//! any other contract, such as a contract that a document names, from its
//! pinned contract document alone:
//!
//! - [`contract`] parses a contract document: the message formats of its
//!   members, the enums and the marks of their fields, the marks of its
//!   members, its failure kinds and the comments of its text.
//! - [`MessageCodec`] converts between the canonical JSON of a message
//!   format ([`value`]) and its wire bytes. The bytes it produces are the
//!   bytes a generated node produces for the same values: both allocate the
//!   same objects in declaration order into the same default heap allocator
//!   and frame with the same serializer.
//! - [`consumer`] holds the clients of any topic, service and action, which
//!   make the messaging calls of a generated node with a codec in place of
//!   generated conversions.
//!
//! The codec lays out each message format at run time: it renders the
//! Cap'n Proto schema of the format ([`schema`]), as the code generator of
//! peppy does, runs the Cap'n Proto compiler on it ([`compiler`]) and reads
//! the placement of each field back ([`wire_layout`]).
//!
//! Peppy gives this crate to a node whose manifest sets
//! `execution.codec_api: true`. The MCP server of peppy and the generic
//! commands of the `peppy` binary use it too.

mod codec;
pub mod compiler;
pub mod consumer;
pub mod contract;
mod dynamic;
pub mod schema;
pub mod value;
pub mod wire_layout;

pub use codec::{CodecError, ConversionError, MessageCodec};

use config::node::MessageFormat;

/// The format of a message side that carries a payload: `None` for a side
/// with no format, and for an empty one, which carries no payload either.
pub fn payload_format(format: Option<&MessageFormat>) -> Option<&MessageFormat> {
    format.filter(|format| !format.0.is_empty())
}
