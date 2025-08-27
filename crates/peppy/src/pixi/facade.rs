use crate::{Error, Result};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

/// A facade for pixi operations that provides a simplified interface
/// and centralized error handling
pub struct PixiFacade {
    pixi_path: String,
    working_dir: PathBuf,
}

impl PixiFacade {
    /// Creates a new PixiFacade instance with a working directory
    pub fn new(working_dir: PathBuf) -> Result<Self> {
        let pixi_path = if let Some(path) = option_env!("PIXI_BINARY_PATH") {
            path.to_string()
        } else {
            return Err(Error::PixiError(
                "Pixi not found. Build with: cargo build --features build_pixi".to_string(),
            ));
        };

        Ok(Self {
            pixi_path,
            working_dir: working_dir,
        })
    }

    /// Adds dependencies to a pixi project
    pub fn add_dependencies(&self, dependencies: &[&str]) -> Result<()> {
        let mut args = vec!["add".to_string()];
        args.extend(dependencies.iter().map(|s| s.to_string()));

        self.execute(&args)
            .map_err(|e| Error::PixiError(format!("Failed to add pixi dependencies: {}", e)))?;
        Ok(())
    }

    /// Adds a task to the pixi project
    pub fn add_task(&self, task_name: &str, task_command: &str) -> Result<()> {
        let args = vec![
            "task".to_string(),
            "add".to_string(),
            task_name.to_string(),
            task_command.to_string(),
        ];

        self.execute(&args).map_err(|e| {
            Error::PixiError(format!("Failed to add pixi task '{}': {}", task_name, e))
        })
    }

    /// Installs dependencies for a pixi project
    pub fn install(&self) -> Result<()> {
        let args = vec!["install".to_string()];
        self.execute(&args)
            .map_err(|e| Error::PixiError(format!("Failed to install pixi dependencies: {}", e)))
    }

    /// Runs a pixi task
    pub fn run_task(&self, task_name: &str) -> Result<()> {
        let args = vec!["run".to_string(), task_name.to_string()];
        self.execute(&args).map_err(|e| {
            Error::PixiError(format!("Failed to run pixi task '{}': {}", task_name, e))
        })
    }

    /// Initializes a new pixi project in the working directory
    pub fn init(&self) -> Result<()> {
        let args = vec!["init".to_string()];
        self.execute(&args)
            .map_err(|e| Error::PixiError(format!("Failed to initialize pixi project: {}", e)))
    }

    /// Internal method to execute pixi commands with proper error handling
    fn execute(&self, args: &[String]) -> Result<()> {
        let mut command = Command::new(&self.pixi_path);
        command.current_dir(&self.working_dir);
        command.args(args);

        let status = command.status().map_err(|e| {
            Error::PixiError(format!("Failed to execute pixi command {:?}: {}", args, e))
        })?;

        if !status.success() {
            return Err(Error::PixiError(format!(
                "Pixi command failed with exit code: {}",
                status.code().unwrap_or(-1)
            )));
        }

        Ok(())
    }

    /// Executes arbitrary pixi commands and returns the exit status
    /// This is primarily used for the CLI proxy to pass through commands to pixi
    pub fn execute_with_status(&self, args: &[String]) -> Result<ExitStatus> {
        let mut command = Command::new(&self.pixi_path);
        command.current_dir(&self.working_dir);
        command.args(args);

        command.status().map_err(|e| {
            Error::PixiError(format!("Failed to execute pixi command {:?}: {}", args, e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pixi_facade_creation() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let facade = PixiFacade::new(temp_dir.path().to_path_buf());
        assert!(facade.is_ok());
    }

    #[test]
    fn test_add_dependencies() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let facade = PixiFacade::new(temp_dir.path().to_path_buf()).unwrap();

        // Initialize a pixi project first
        let _ = facade.init();

        // Test adding single dependency
        let result = facade.add_dependencies(&["python"]);
        assert!(result.is_ok());

        // Test adding multiple dependencies
        let result = facade.add_dependencies(&["numpy", "pandas"]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_add_task() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let facade = PixiFacade::new(temp_dir.path().to_path_buf()).unwrap();

        // Initialize a pixi project first
        let _ = facade.init();

        // Test adding a task
        let result = facade.add_task("test", "echo 'Running tests'");
        assert!(result.is_ok());

        // Test adding another task
        let result = facade.add_task("build", "echo 'Building project'");
        assert!(result.is_ok());
    }

    #[test]
    fn test_install() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let facade = PixiFacade::new(temp_dir.path().to_path_buf()).unwrap();

        // Initialize a pixi project first
        let _ = facade.init();

        // Test install command
        let result = facade.install();
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_task() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let facade = PixiFacade::new(temp_dir.path().to_path_buf()).unwrap();

        // Initialize a pixi project and add a task
        let _ = facade.init();
        let _ = facade.add_task("hello", "echo 'Hello World'");

        // Test running the task
        let result = facade.run_task("hello");
        assert!(result.is_ok());
    }

    #[test]
    fn test_init() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let facade = PixiFacade::new(temp_dir.path().to_path_buf()).unwrap();

        // Test init command
        let result = facade.init();
        assert!(result.is_ok());

        // Verify pixi.toml was created
        let pixi_toml = temp_dir.path().join("pixi.toml");
        assert!(pixi_toml.exists());
    }

    #[test]
    fn test_execute_with_status() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let facade = PixiFacade::new(temp_dir.path().to_path_buf()).unwrap();

        // Test command that should succeed
        let result = facade.execute_with_status(&["--version".to_string()]);
        assert!(result.is_ok());
        let status = result.unwrap();
        assert!(status.success());

        // Test command that should fail (invalid command)
        let result = facade.execute_with_status(&["invalid-command".to_string()]);
        if let Ok(status) = result {
            assert!(!status.success());
        }
    }

    #[test]
    fn test_error_handling() {
        let facade = PixiFacade::new(PathBuf::from("/nonexistent")).unwrap();

        // Test adding dependencies without a valid project
        let result = facade.add_dependencies(&["python"]);
        assert!(result.is_err());

        // Test running non-existent task with different facade instance
        let temp_facade = PixiFacade::new(PathBuf::from("/tmp")).unwrap();
        let result = temp_facade.run_task("nonexistent");
        assert!(result.is_err());
    }
}
