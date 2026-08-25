use crate::error::{Error, Result};

/// `peppy mcp serve`: runs the built-in server until the daemon stops it.
/// Everything it serves comes from the environment the daemon set.
pub(super) fn mcp_serve() -> Result<()> {
    mcp_server::serve().map_err(|error| Error::ExecutionFailed(error.to_string()))
}
