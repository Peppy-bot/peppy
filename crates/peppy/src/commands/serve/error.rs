#[derive(thiserror::Error, Debug)]
pub enum ServeCommandError {
    #[error("Unsupported configuration engine. Supported options are 'zenoh'/'mock'")]
    UnsupportedEngine,
}
