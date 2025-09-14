use super::ServeAsyncCommand;
use crate::{InterfacesGenerator, Result};
use tokio::task::JoinHandle;

impl ServeAsyncCommand for InterfacesGenerator {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        todo!("Finish")
    }
}
