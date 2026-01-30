pub mod info;
pub mod node;
pub mod service;
pub mod stack;

use std::future::Future;
use std::sync::Arc;

use crate::{context::AppContext, error::Result};

/// Trait for executable commands
pub trait Command {
    /// Execute the command
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()>;
}

pub(crate) fn block_on<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(future)
        }
    }
}
