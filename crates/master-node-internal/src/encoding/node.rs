//! Cap'n Proto encoding utilities for node messages.
pub mod add;
pub mod init;
pub mod list;
pub mod remove;
pub mod run;
pub mod shutdown;
pub mod sync;
pub mod templates;

pub use super::{decode_message, encode_message};
