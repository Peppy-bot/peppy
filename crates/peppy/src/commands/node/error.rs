#[derive(thiserror::Error, Debug)]
pub enum NodeCommandError {
    #[error("Root configuration not found")]
    RootConfigurationNotFound,

    #[error("Unsupported configuration language. Supported options are 'python'/'rust'")]
    UnsupportedLanguage,

    #[error("Folder already exists at path: {0}")]
    FolderAlreadyExist(String),

    #[error(
        "Invalid node name: {0}. Node names must start with a letter and contain only alphanumeric characters, underscores, or hyphens"
    )]
    InvalidNodeName(String),

    #[error("Failed to create git configuration: {0}")]
    GitConfigCreation(String),

    #[error("Failed to create peppy configuration: {0}")]
    PeppyConfigCreation(String),

    #[error("Failed to create pixi configuration: {0}")]
    PixiConfigCreation(String),

    #[error("Failed to create Rust configuration: {0}")]
    RustConfigCreation(String),

    #[error("Failed to create Python configuration: {0}")]
    PythonConfigCreation(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}
