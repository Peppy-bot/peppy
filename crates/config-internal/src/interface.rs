mod parse;
mod types;

// Defines the parsing of interface documents (`peppy_schema: "interface_v1"`).
// Interface files are stand-alone JSON5 documents that declare a reusable
// contract — the topics, services, and actions a node claims to expose.
// Filenames are not fixed; any `.json5` whose body carries the
// `interface_v1` schema tag is an interface.
pub use parse::PeppyInterfaceParser;
pub(crate) use types::validate_named_items;
pub use types::{Interfaces, Manifest, PeppyInterface};
