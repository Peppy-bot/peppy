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
    Setup(peppylib::PeppyError),
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
        peppylib::checker::check_node_config_up_to_date(config_path, crate_root);
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    const CONFIG_CONTENTS: &str = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            launch_cmd: ["cargo", "run", "--release"],
        },
    }
    "#;

    // This hash is precomputed based on CONFIG_CONTENTS above
    const CONFIG_FINGERPRINT: &str =
        "b64ecd7e3c9b2d170535598b370c865cd320bc93d93b98aa18c0e999046f1008";

    #[test]
    fn init_node_can_start() {
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let _fixture = NodeConfigFixture::install(&crate_root, CONFIG_CONTENTS, CONFIG_FINGERPRINT);

        let runtime = tokio::runtime::Runtime::new().expect("failed to build runtime for test");
        runtime
            .block_on(super::init_node())
            .expect("init_node should succeed with valid config");
    }

    struct NodeConfigFixture {
        config_path: PathBuf,
        fingerprint_path: PathBuf,
        fingerprint_dir: PathBuf,
        original_config: Option<Vec<u8>>,
        original_fingerprint: Option<Vec<u8>>,
        had_fingerprint_dir: bool,
    }

    impl NodeConfigFixture {
        fn install(crate_root: &Path, config_contents: &str, fingerprint: &str) -> Self {
            let config_path = crate_root.join(super::NODE_CONFIG_FILE);
            let fingerprint_dir = crate_root.join(".peppygen");
            let fingerprint_path = fingerprint_dir.join("node_config.sha256");

            let original_config = fs::read(&config_path).ok();
            let original_fingerprint = fs::read(&fingerprint_path).ok();
            let had_fingerprint_dir = fingerprint_dir.exists();

            fs::create_dir_all(&fingerprint_dir)
                .expect("failed to create .peppygen directory for test");

            fs::write(&config_path, config_contents)
                .expect("failed to write config fixture for init_node test");
            fs::write(&fingerprint_path, format!("{fingerprint}\n"))
                .expect("failed to write fingerprint fixture for init_node test");

            Self {
                config_path,
                fingerprint_path,
                fingerprint_dir,
                original_config,
                original_fingerprint,
                had_fingerprint_dir,
            }
        }
    }

    impl Drop for NodeConfigFixture {
        fn drop(&mut self) {
            if let Some(bytes) = self.original_config.as_ref() {
                let _ = fs::write(&self.config_path, bytes);
            } else {
                let _ = fs::remove_file(&self.config_path);
            }

            if let Some(bytes) = self.original_fingerprint.as_ref() {
                let _ = fs::write(&self.fingerprint_path, bytes);
            } else {
                let _ = fs::remove_file(&self.fingerprint_path);
            }

            if !self.had_fingerprint_dir {
                let _ = fs::remove_dir(&self.fingerprint_dir);
            }
        }
    }
}
