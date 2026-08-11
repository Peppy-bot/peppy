mod parse;
mod types;

// Defines the parsing of contract documents (`peppy_schema: "contract/v1"`).
// Contract files are stand-alone JSON5 documents that declare a reusable
// contract: the topics, services, and actions a node claims to expose.
// Filenames are not fixed; any `.json5` whose body carries the
// `contract/v1` schema tag is a contract.
pub use parse::PeppyContractParser;
pub use types::{Interfaces, Manifest, PeppyContract};
pub(crate) use types::{deserialize_tag, validate_named_items};
