pub mod error;
mod generator;

// Exposes all the generated interfaces
pub mod interfaces;

pub use generator::generate_interfaces_code;
