mod parse;
mod types;

// Defines the parsing of pairing documents (`peppy_schema: "pairing/v1"`).
// A pairing is a named, two-role, topics-only contract two node instances
// pair 1:1 over; the document declares the two roles and one flat topics
// list where every topic carries `emitted_by: <role>`. Filenames are not
// fixed; any `.json5` whose body carries the `pairing/v1` schema tag is a
// pairing.
pub use parse::PeppyPairingParser;
pub use types::{PairingTopic, PeppyPairing};
