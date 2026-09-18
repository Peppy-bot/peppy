//! Pretty-print a `Serialize` value as JSON5 with unquoted object keys.
//!
//! `serde_json::to_string_pretty` always quotes keys, which produces
//! valid JSON5 but loses the lighter style used elsewhere in this
//! workspace (see `default_repositories.json5`). This helper round-trips
//! through `serde_json::Value` and emits keys without quotes when they
//! match the JSON5 identifier grammar.

mod ident;
mod writer;

pub use writer::to_string_pretty;
