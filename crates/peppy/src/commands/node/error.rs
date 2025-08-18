use thiserror::Error;

#[derive(Error, Debug)]
pub enum NodeCreationError {
    #[error("Failed to create directory: {0}")]
    DirectoryCreation(#[from] std::io::Error),

    #[error("Failed to get current directory")]
    CurrentDir(std::io::Error),

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
}
