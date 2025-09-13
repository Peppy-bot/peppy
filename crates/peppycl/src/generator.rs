// Module that generates interfaces based on the exposes category of the config
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Language {
    Python,
    Rust,
}

pub fn generate_exposes_interfaces(lang: Language) {}
