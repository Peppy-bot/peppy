use std::path::PathBuf;

#[derive(Clone)]
pub struct MessagingEngineContext {
    pub engine: String,
    pub config_path: Option<PathBuf>,
}

impl MessagingEngineContext {
    pub fn new(engine: String, config_path: Option<PathBuf>) -> Self {
        Self {
            engine,
            config_path,
        }
    }
}
