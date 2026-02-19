use serde::de::DeserializeOwned as SerdeDeserializeOwned;
use serde::{Deserialize as SerdeDeserialize, Serialize as SerdeSerialize};

// Re-export derive macros
pub use schemars_derive::JsonSchema;
pub use serde_derive::{Deserialize, Serialize};

/// Wrapper trait for `serde::Serialize`.
pub trait Serialize: SerdeSerialize {}
impl<T: ?Sized + SerdeSerialize> Serialize for T {}

/// Wrapper trait for `serde::Deserialize`.
pub trait Deserialize<'de>: SerdeDeserialize<'de> {}
impl<'de, T: SerdeDeserialize<'de>> Deserialize<'de> for T {}

/// Wrapper trait for `serde::de::DeserializeOwned`.
pub trait DeserializeOwned: SerdeDeserializeOwned {}
impl<T: SerdeDeserializeOwned> DeserializeOwned for T {}

/// Wrapper trait for `schemars::JsonSchema`.
pub trait JsonSchema: schemars::JsonSchema {}
impl<T: ?Sized + schemars::JsonSchema> JsonSchema for T {}
