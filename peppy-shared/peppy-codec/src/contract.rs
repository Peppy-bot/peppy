//! The parser of contract documents (`peppy_schema: "contract/v1"`).
//!
//! A contract is a stand-alone JSON5 document that declares the topics,
//! services and actions a node claims to expose. Filenames are not fixed:
//! any `.json5` whose body carries the `contract/v1` schema tag is a
//! contract. [`PeppyContractParser`] gives a [`PeppyContract`]: the
//! description of the contract, the description and the message formats of
//! each member, with the enums (`$enum`), the marks (`$marks`) and the
//! descriptions (`$description`) of their fields, the marks of each member
//! and the failure kinds of the contract.

mod parse;
mod side;
mod types;

pub use parse::{ContractError, PeppyContractParser};
pub use side::MessageSide;
pub use types::{Interfaces, Manifest, PeppyContract, RefinedMember};

// The types of the config model that the API of a contract names, so a node
// that links this crate alone can name them.
pub use config::description::{Description, DocumentDescription};
pub use config::failure_kind::{MemberFailureKinds, MemberSection};
pub use config::mark::{EnumValues, MOVES_ROBOT, MarkName, Marks};
pub use config::node::{InterfaceKind, MessageFormat, SchemaType};
