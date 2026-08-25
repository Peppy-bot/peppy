//! Conversion between canonical exposure JSON and Peppy wire messages,
//! driven by a contract's `message_format` at run time, and the
//! type-erased topic, service and action clients that use it.
//!
//! A [`MessageCodec`] compiles the message format through the bundled
//! Cap'n Proto compiler and walks the compiler's layout to read and write
//! messages. The bytes it produces are the bytes a generated node produces
//! for the same values: both allocate the same objects in declaration order
//! into the same default heap allocator and frame with the same serializer.
//! The JSON side applies the canonical value rules of
//! [`peppy_mcp_runtime::bridge`], the rules generated bridges apply.

mod codec;
pub mod consumer;
mod dynamic;

pub use codec::{CodecError, ConversionError, MessageCodec};
