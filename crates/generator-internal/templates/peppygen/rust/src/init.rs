use std::{
    any::Any,
    fmt,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
};

const NODE_CONFIG_FILE: &str = "peppy.json5";

#[derive(Debug)]
pub enum InitNodeError {
    OutOfSyncNode { message: String },
    RuntimeInit(std::io::Error),
    Setup(peppylib::ControlError),
}

pub type InitNodeResult<T> = Result<T, InitNodeError>;

impl fmt::Display for InitNodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfSyncNode { message } => f.write_str(message),
            Self::RuntimeInit(err) => write!(f, "failed to initialize async runtime: {err}"),
            Self::Setup(err) => write!(f, "failed to set up node: {err}"),
        }
    }
}

impl std::error::Error for InitNodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OutOfSyncNode { .. } => None,
            Self::RuntimeInit(err) => Some(err),
            Self::Setup(err) => Some(err),
        }
    }
}

pub async fn init_node() -> InitNodeResult<()> {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config_path = crate_root.join(NODE_CONFIG_FILE);

    ensure_config_is_in_sync(&config_path, &crate_root)?;
    peppylib::setup_node(Some(config_path))
        .await
        .map_err(InitNodeError::Setup)
}

pub fn init_node_blocking() -> InitNodeResult<()> {
    let runtime = tokio::runtime::Runtime::new().map_err(InitNodeError::RuntimeInit)?;
    runtime.block_on(init_node())
}

fn ensure_config_is_in_sync(config_path: &Path, crate_root: &Path) -> InitNodeResult<()> {
    match panic::catch_unwind(AssertUnwindSafe(|| {
        crate::checker::check_node_config_up_to_date(config_path, crate_root);
    })) {
        Ok(()) => Ok(()),
        Err(payload) => Err(InitNodeError::OutOfSyncNode {
            message: format_out_of_sync_message(payload, config_path, crate_root),
        }),
    }
}

fn format_out_of_sync_message(
    payload: Box<dyn Any + Send>,
    config_path: &Path,
    crate_root: &Path,
) -> String {
    let panic_message = match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => String::from("generated bindings are out of sync with node configuration"),
        },
    };

    format!(
        "{panic_message}\nGenerated crate: `{}`\nNode config: `{}`",
        crate_root.display(),
        config_path.display()
    )
}
