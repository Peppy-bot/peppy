//! The parser of contract documents (`peppy_schema: "contract/v1"`).
//!
//! A contract is a stand-alone JSON5 document that declares the topics,
//! services and actions a node claims to expose. Filenames are not fixed:
//! any `.json5` whose body carries the `contract/v1` schema tag is a
//! contract. [`PeppyContractParser`] gives a [`PeppyContract`]: the message
//! formats of each member, with the enums (`$enum`) and the marks (`$marks`)
//! of their fields, the marks of each member, the failure kinds of the
//! contract, and the comments of the text ([`ContractComments`]).

mod comments;
mod parse;
mod types;

pub use comments::{ContractComments, MessageSide};
pub use parse::{ContractError, PeppyContractParser};
pub use types::{Interfaces, Manifest, PeppyContract, RefinedMember};
