// Integration tests for documentation snippets

pub mod serve;
pub mod setup;

pub use serve::TestServeHandle;
pub use setup::{peppy_binary, workspace_root};
